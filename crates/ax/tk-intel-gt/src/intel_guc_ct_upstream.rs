// SPDX-License-Identifier: MIT
// Copyright © 2016-2019 Intel Corporation.
// Source: Linux v7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_ct.c.
// CTB memory and transport logic is translated here; request/event handlers
// remain calls into the owning GuC submission, log, capture, and TLB modules.

#![allow(dead_code, non_snake_case, unsafe_code)]

use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of},
    ptr,
};

use crate::{
    i915_gem_object_api_upstream::i915_gem_object_has_pinned_pages,
    i915_scheduler_types_upstream::TaskletStruct,
    i915_vma_api_upstream::{I915_VMA_RELEASE_MAP, i915_vma_unpin_and_release},
    i915_vma_types_upstream::I915Vma,
    intel_engine_cs_upstream::{ListHead, WorkStruct},
    intel_gt_api_upstream::{guc_to_gt, uc_to_gt},
    intel_guc_actions_abi_types_upstream::{
        GUC_ACTION_HOST2GUC_CONTROL_CTB, GUC_CTB_CONTROL_DISABLE, GUC_CTB_CONTROL_ENABLE,
        intel_guc_action,
    },
    intel_guc_ct_types_upstream::{GucCtBufferDesc, IntelGucCt, IntelGucCtBuffer},
    intel_guc_types_upstream::{IntelGuc, intel_guc_is_fw_running},
    intel_guc_upstream::{
        intel_guc_allocate_and_map_vma, intel_guc_crash_process_msg, intel_guc_notify,
        intel_guc_self_cfg32, intel_guc_self_cfg64, intel_guc_to_host_process_recv_msg,
        intel_guc_write_barrier,
    },
    intel_uc_types_upstream::IntelUc,
    linux::{
        config::{EBADMSG, EBUSY, EIO, EMSGSIZE, ENODEV, EOPNOTSUPP, EPIPE, EPROTO, GFP_ATOMIC},
        locks::{
            spin_lock, spin_lock_init, spin_lock_irqsave, spin_unlock, spin_unlock_irqrestore,
        },
        memory::{atomic_add, atomic_read, atomic_set, kfree, kmalloc},
        primitives::ktime_get,
        registers::{REG_FIELD_GET, REG_FIELD_PREP},
        tasklet::{tasklet_hi_schedule, tasklet_kill, tasklet_setup},
        wait::{init_waitqueue_head, msleep},
        workqueue::{INIT_WORK_C, queue_work, system_dfl_wq},
    },
    linux_print::{DrmPrinter, drm_printer_write},
};

const CTB_H2G_BUFFER_SIZE: u32 = crate::guc_ct::CTB_H2G_BUFFER_SIZE as u32;
const CTB_G2H_BUFFER_SIZE: u32 = crate::guc_ct::CTB_G2H_BUFFER_SIZE as u32;
const CTB_DESC_SIZE: u32 = crate::guc_ct::CTB_DESC_SIZE as u32;
const G2H_ROOM_BUFFER_SIZE: u32 = crate::guc_ct::G2H_ROOM_BUFFER_SIZE as u32;
const GUC_CTB_HDR_LEN: u32 = crate::guc_ct::GUC_CTB_HDR_LEN as u32;
const GUC_CTB_MSG_MIN_LEN: u32 = crate::guc_ct::GUC_CTB_MSG_MIN_LEN as u32;
const GUC_CTB_MSG_MAX_LEN: u32 = crate::guc_ct::GUC_CTB_MSG_MAX_LEN as u32;
const GUC_CTB_HXG_MSG_MIN_LEN: u32 = crate::guc_ct::GUC_CTB_HXG_MSG_MIN_LEN as u32;
const GUC_CTB_HXG_MSG_MAX_LEN: u32 = crate::guc_ct::GUC_CTB_HXG_MSG_MAX_LEN as u32;
const GUC_CTB_MSG_0_FENCE: u32 = crate::guc_ct::GUC_CTB_MSG_0_FENCE;
const GUC_CTB_MSG_0_FORMAT: u32 = crate::guc_ct::GUC_CTB_MSG_0_FORMAT;
const GUC_CTB_MSG_0_NUM_DWORDS: u32 = crate::guc_ct::GUC_CTB_MSG_0_NUM_DWORDS;
const GUC_CTB_FORMAT_HXG: u32 = crate::guc_ct::GUC_CTB_FORMAT_HXG;
const GUC_CTB_STATUS_UNUSED: u32 = crate::guc_ct::GUC_CTB_STATUS_UNUSED;
const GUC_CTB_STATUS_OVERFLOW: u32 = crate::guc_ct::GUC_CTB_STATUS_OVERFLOW;
const GUC_CTB_STATUS_UNDERFLOW: u32 = crate::guc_ct::GUC_CTB_STATUS_UNDERFLOW;
const GUC_CTB_STATUS_MISMATCH: u32 = crate::guc_ct::GUC_CTB_STATUS_MISMATCH;
const INTEL_GUC_CT_SEND_NB: u32 = crate::guc_ct::CT_SEND_NB;
const INTEL_GUC_CT_SEND_G2H_DW_MASK: u32 = crate::guc_ct::CT_SEND_G2H_DW_MASK;
const GUC_HXG_MSG_MIN_LEN: u32 = crate::guc_ct::GUC_HXG_MSG_MIN_LEN as u32;
const GUC_HXG_MSG_0_ORIGIN: u32 = crate::guc_ct::GUC_HXG_MSG_0_ORIGIN;
const GUC_HXG_MSG_0_TYPE: u32 = crate::guc_ct::GUC_HXG_MSG_0_TYPE;
const GUC_HXG_REQUEST_MSG_0_ACTION: u32 = crate::guc_ct::GUC_HXG_REQUEST_MSG_0_ACTION;
const GUC_HXG_EVENT_MSG_0_ACTION: u32 = crate::guc_ct::GUC_HXG_EVENT_MSG_0_ACTION;
const GUC_HXG_ORIGIN_HOST: u32 = crate::guc_ct::HXG_ORIGIN_HOST;
const GUC_HXG_ORIGIN_GUC: u32 = crate::guc_ct::HXG_ORIGIN_GUC;
const GUC_HXG_TYPE_EVENT: u32 = crate::guc_ct::HXG_TYPE_EVENT;
const GUC_HXG_TYPE_FAST_REQUEST: u32 = crate::guc_ct::HXG_TYPE_FAST_REQUEST;
const GUC_HXG_TYPE_REQUEST: u32 = crate::guc_ct::HXG_TYPE_REQUEST;
const GUC_HXG_TYPE_NO_RESPONSE_RETRY: u32 = crate::guc_ct::HXG_TYPE_NO_RESPONSE_RETRY;
const GUC_HXG_TYPE_RESPONSE_SUCCESS: u32 = crate::guc_ct::HXG_TYPE_RESPONSE_SUCCESS;
const GUC_HXG_TYPE_RESPONSE_FAILURE: u32 = crate::guc_ct::HXG_TYPE_RESPONSE_FAILURE;
const GUC_HXG_RESPONSE_MSG_0_DATA0: u32 = crate::guc_ct::GUC_HXG_RESPONSE_MSG_0_DATA0;
const GUC_HXG_RETRY_MSG_0_REASON: u32 = crate::guc_ct::GUC_HXG_RETRY_MSG_0_REASON;
const INTEL_GUC_ACTION_HOST2GUC_CONTROL_CTB: u32 = GUC_ACTION_HOST2GUC_CONTROL_CTB;
const GUC_CTB_RESPONSE_TIMEOUT_SHORT_MS: u32 = 10;
const GUC_CTB_RESPONSE_TIMEOUT_LONG_MS: u32 = 1000;
const GUC_CTB_TIMEOUT_MS: i64 = 1500;
const ENOKEY_LOCAL: i32 = 126;
const EOPNOTSUPP_LOCAL: i32 = EOPNOTSUPP;
const EMSGSIZE_LOCAL: i32 = EMSGSIZE;
const GUC_CTB_STATUS_NO_ERROR: u32 = crate::guc_ct::GUC_CTB_STATUS_NO_ERROR;
const INTEL_GUC_ACTION_DEFAULT: u32 = intel_guc_action::INTEL_GUC_ACTION_DEFAULT as u32;
const INTEL_GUC_ACTION_CONTEXT_RESET_NOTIFICATION: u32 =
    intel_guc_action::INTEL_GUC_ACTION_CONTEXT_RESET_NOTIFICATION as u32;
const INTEL_GUC_ACTION_DEREGISTER_CONTEXT_DONE: u32 =
    intel_guc_action::INTEL_GUC_ACTION_DEREGISTER_CONTEXT_DONE as u32;
const INTEL_GUC_ACTION_ENGINE_FAILURE_NOTIFICATION: u32 =
    intel_guc_action::INTEL_GUC_ACTION_ENGINE_FAILURE_NOTIFICATION as u32;
const INTEL_GUC_ACTION_NOTIFY_CRASH_DUMP_POSTED: u32 =
    intel_guc_action::INTEL_GUC_ACTION_NOTIFY_CRASH_DUMP_POSTED as u32;
const INTEL_GUC_ACTION_NOTIFY_EXCEPTION: u32 =
    intel_guc_action::INTEL_GUC_ACTION_NOTIFY_EXCEPTION as u32;
const INTEL_GUC_ACTION_NOTIFY_FLUSH_LOG_BUFFER_TO_FILE: u32 =
    intel_guc_action::INTEL_GUC_ACTION_NOTIFY_FLUSH_LOG_BUFFER_TO_FILE as u32;
const INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_DONE: u32 =
    intel_guc_action::INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_DONE as u32;
const INTEL_GUC_ACTION_STATE_CAPTURE_NOTIFICATION: u32 =
    intel_guc_action::INTEL_GUC_ACTION_STATE_CAPTURE_NOTIFICATION as u32;
const INTEL_GUC_ACTION_TLB_INVALIDATION_DONE: u32 =
    intel_guc_action::INTEL_GUC_ACTION_TLB_INVALIDATION_DONE as u32;

#[repr(C, packed)]
struct GucCtBufferDescLayout {
    head: u32,
    tail: u32,
    status: u32,
    reserved: [u32; 13],
}

#[repr(C)]
struct CtRequest {
    link: ListHead,
    fence: u32,
    status: u32,
    response_len: u32,
    response_buf: *mut u32,
}

#[repr(C)]
struct CtIncomingMsg {
    link: ListHead,
    size: u32,
    msg: [u32; 0],
}

const _: [(); 64] = [(); size_of::<GucCtBufferDescLayout>()];
const _: [(); 0] = [(); offset_of!(CtIncomingMsg, link)];

#[inline]
unsafe fn ct_msg_data(msg: *mut CtIncomingMsg) -> *mut u32 {
    unsafe { (msg.cast::<u8>().add(offset_of!(CtIncomingMsg, msg))).cast() }
}

unsafe extern "C" {
    fn intel_guc_ct_send_mmio(
        guc: *mut IntelGuc,
        action: *const u32,
        len: u32,
        response: *mut u32,
        response_len: u32,
    ) -> i32;
    fn intel_guc_deregister_done_process_msg(guc: *mut IntelGuc, msg: *const u32, len: u32) -> i32;
    fn intel_guc_sched_done_process_msg(guc: *mut IntelGuc, msg: *const u32, len: u32) -> i32;
    fn intel_guc_context_reset_process_msg(guc: *mut IntelGuc, msg: *const u32, len: u32) -> i32;
    fn intel_guc_error_capture_process_msg(guc: *mut IntelGuc, msg: *const u32, len: u32) -> i32;
    fn intel_guc_engine_failure_process_msg(guc: *mut IntelGuc, msg: *const u32, len: u32) -> i32;
    fn intel_guc_log_handle_flush_event(log: *mut crate::intel_guc_log_types_upstream::IntelGucLog);
    fn intel_guc_tlb_invalidation_done(guc: *mut IntelGuc, msg: *const u32, len: u32) -> i32;
    fn intel_guc_fast_response_selftest(guc: *mut IntelGuc) -> bool;
}

macro_rules! CT_ERROR {
    ($ct:expr, $format:literal $(, $arg:expr)* $(,)?) => {{
        guc_err!(unsafe { ct_to_guc($ct) }, concat!("CT: ", $format) $(, $arg)*);
    }};
}

macro_rules! CT_DEBUG {
    ($ct:expr, $format:literal $(, $arg:expr)* $(,)?) => {{
        guc_dbg!(unsafe { ct_to_guc($ct) }, concat!("CT: ", $format) $(, $arg)*);
    }};
}

macro_rules! CT_PROBE_ERROR {
    ($ct:expr, $format:literal $(, $arg:expr)* $(,)?) => {{
        guc_probe_error!(unsafe { ct_to_guc($ct) }, concat!("CT: ", $format) $(, $arg)*);
    }};
}

macro_rules! CT_DEAD {
    ($ct:expr, $reason:expr) => {{
        // CONFIG_DRM_I915_DEBUG is disabled for this target, so the upstream
        // dead-CT worker bookkeeping compiles out exactly as CT_DEAD does.
        let _ = $ct;
    }};
}

unsafe fn desc_layout(desc: *mut GucCtBufferDesc) -> *mut GucCtBufferDescLayout {
    desc.cast()
}

#[inline]
fn circ_space(tail: u32, head: u32, size: u32) -> u32 {
    head.wrapping_sub(tail).wrapping_sub(1) & (size - 1)
}

#[inline]
fn circ_count(head: u32, tail: u32, size: u32) -> u32 {
    head.wrapping_sub(tail) & (size - 1)
}

// upstream: intel_guc_ct.c ct_to_guc()
unsafe fn ct_to_guc(ct: *mut IntelGucCt) -> *mut IntelGuc {
    container_of!(ct, IntelGuc, ct)
}

// upstream: intel_guc_ct.c intel_guc_ct_max_queue_time_jiffies()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_max_queue_time_jiffies() -> isize {
    ((CTB_H2G_BUFFER_SIZE as isize * crate::linux::config::HZ as isize) / 2048) as isize
}

// upstream: intel_guc_ct.c intel_guc_ct_init_early()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_init_early(ct: *mut IntelGucCt) {
    unsafe {
        spin_lock_init(&mut (*ct).ctbs.send.lock);
        spin_lock_init(&mut (*ct).ctbs.recv.lock);
        spin_lock_init(&mut (*ct).requests.lock);
        crate::linux::list::INIT_LIST_HEAD(ptr::addr_of_mut!((*ct).requests.pending));
        crate::linux::list::INIT_LIST_HEAD(ptr::addr_of_mut!((*ct).requests.incoming));
        INIT_WORK_C(&mut (*ct).requests.worker, ct_incoming_request_worker_func);
        tasklet_setup(
            ptr::addr_of_mut!((*ct).receive_tasklet),
            ct_receive_tasklet_func,
        );
        init_waitqueue_head(&mut (*ct).wq);
    }
}

// upstream: intel_guc_ct.c guc_ct_buffer_desc_init()
unsafe fn guc_ct_buffer_desc_init(desc: *mut GucCtBufferDesc) {
    unsafe { ptr::write_bytes(desc.cast::<u8>(), 0, size_of::<GucCtBufferDescLayout>()) };
}

// upstream: intel_guc_ct.c guc_ct_buffer_reset()
unsafe fn guc_ct_buffer_reset(ctb: *mut IntelGucCtBuffer) {
    let desc = unsafe { desc_layout((*ctb).desc) };
    unsafe {
        (*ctb).broken = false;
        (*ctb).tail = 0;
        (*ctb).head = 0;
        let space = circ_space((*ctb).tail, (*ctb).head, (*ctb).size) - (*ctb).resv_space;
        atomic_set(&mut (*ctb).space, space as i32);
        guc_ct_buffer_desc_init((*ctb).desc);
        let _ = desc;
    }
}

// upstream: intel_guc_ct.c guc_ct_buffer_init()
unsafe fn guc_ct_buffer_init(
    ctb: *mut IntelGucCtBuffer,
    desc: *mut GucCtBufferDesc,
    cmds: *mut u32,
    size_in_bytes: u32,
    resv_space: u32,
) {
    GEM_BUG_ON!(size_in_bytes % 4 != 0);
    unsafe {
        (*ctb).desc = desc;
        (*ctb).cmds = cmds;
        (*ctb).size = size_in_bytes / 4;
        (*ctb).resv_space = resv_space / 4;
        guc_ct_buffer_reset(ctb);
    }
}

// upstream: intel_guc_ct.c guc_action_control_ctb()
unsafe fn guc_action_control_ctb(guc: *mut IntelGuc, control: u32) -> i32 {
    GEM_BUG_ON!(control != GUC_CTB_CONTROL_DISABLE && control != GUC_CTB_CONTROL_ENABLE);
    let request = [
        REG_FIELD_PREP(
            crate::guc_ct::GUC_HXG_MSG_0_TYPE,
            crate::guc_ct::HXG_TYPE_REQUEST,
        ) | REG_FIELD_PREP(
            crate::guc_ct::GUC_HXG_REQUEST_MSG_0_ACTION,
            INTEL_GUC_ACTION_HOST2GUC_CONTROL_CTB,
        ),
        control,
    ];
    let ret = unsafe {
        crate::intel_guc_upstream::intel_guc_send_mmio(
            guc,
            request.as_ptr(),
            request.len() as u32,
            ptr::null_mut(),
            0,
        )
    };
    if ret > 0 { -EPROTO } else { ret }
}

// upstream: intel_guc_ct.c ct_control_enable()
unsafe fn ct_control_enable(ct: *mut IntelGucCt, enable: bool) -> i32 {
    let control = if enable {
        GUC_CTB_CONTROL_ENABLE
    } else {
        GUC_CTB_CONTROL_DISABLE
    };
    let err = unsafe { guc_action_control_ctb(ct_to_guc(ct), control) };
    if err != 0 {
        CT_PROBE_ERROR!(
            ct,
            "Failed to control/%s CTB (%pe)\n",
            if enable { "enable" } else { "disable" },
            crate::linux::config::ERR_PTR::<c_void>(err)
        );
    }
    err
}

// upstream: intel_guc_ct.c ct_register_buffer()
unsafe fn ct_register_buffer(
    ct: *mut IntelGucCt,
    send: bool,
    desc_addr: u32,
    buff_addr: u32,
    size: u32,
) -> i32 {
    let guc = unsafe { ct_to_guc(ct) };
    let (desc_key, addr_key, size_key) = if send {
        (
            crate::guc_ct::KLV_SELF_CFG_H2G_CTB_DESCRIPTOR_ADDR,
            crate::guc_ct::KLV_SELF_CFG_H2G_CTB_ADDR,
            crate::guc_ct::KLV_SELF_CFG_H2G_CTB_SIZE,
        )
    } else {
        (
            crate::guc_ct::KLV_SELF_CFG_G2H_CTB_DESCRIPTOR_ADDR,
            crate::guc_ct::KLV_SELF_CFG_G2H_CTB_ADDR,
            crate::guc_ct::KLV_SELF_CFG_G2H_CTB_SIZE,
        )
    };
    let mut err = unsafe { intel_guc_self_cfg64(guc, desc_key, desc_addr as u64) };
    if err == 0 {
        err = unsafe { intel_guc_self_cfg64(guc, addr_key, buff_addr as u64) };
    }
    if err == 0 {
        err = unsafe { intel_guc_self_cfg32(guc, size_key, size) };
    }
    if err != 0 {
        CT_PROBE_ERROR!(
            ct,
            "Failed to register %s buffer (%pe)\n",
            if send { "SEND" } else { "RECV" },
            crate::linux::config::ERR_PTR::<c_void>(err)
        );
    }
    err
}

// upstream: intel_guc_ct.c intel_guc_ct_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_init(ct: *mut IntelGucCt) -> i32 {
    let guc = unsafe { ct_to_guc(ct) };
    GEM_BUG_ON!(unsafe { !(*ct).vma.is_null() });
    let blob_size = 2 * CTB_DESC_SIZE + CTB_H2G_BUFFER_SIZE + CTB_G2H_BUFFER_SIZE;
    let mut blob = ptr::null_mut();
    let err = unsafe {
        intel_guc_allocate_and_map_vma(guc, blob_size, ptr::addr_of_mut!((*ct).vma), &mut blob)
    };
    if err != 0 {
        CT_PROBE_ERROR!(
            ct,
            "Failed to allocate %u for CTB data (%pe)\n",
            blob_size,
            crate::linux::config::ERR_PTR::<c_void>(err)
        );
        return err;
    }
    CT_DEBUG!(
        ct,
        "base=%#x size=%u\n",
        unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*ct).vma) },
        blob_size
    );
    let mut desc = blob.cast::<GucCtBufferDesc>();
    let mut cmds = unsafe {
        blob.cast::<u8>()
            .add((2 * CTB_DESC_SIZE) as usize)
            .cast::<u32>()
    };
    unsafe {
        guc_ct_buffer_init(
            ptr::addr_of_mut!((*ct).ctbs.send),
            desc,
            cmds,
            CTB_H2G_BUFFER_SIZE,
            0,
        );
    }

    desc = unsafe {
        blob.cast::<u8>()
            .add(CTB_DESC_SIZE as usize)
            .cast::<GucCtBufferDesc>()
    };
    cmds = unsafe {
        blob.cast::<u8>()
            .add((2 * CTB_DESC_SIZE + CTB_H2G_BUFFER_SIZE) as usize)
            .cast::<u32>()
    };
    unsafe {
        guc_ct_buffer_init(
            ptr::addr_of_mut!((*ct).ctbs.recv),
            desc,
            cmds,
            CTB_G2H_BUFFER_SIZE,
            G2H_ROOM_BUFFER_SIZE,
        );
    }
    0
}

// upstream: intel_guc_ct.c intel_guc_ct_fini()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_fini(ct: *mut IntelGucCt) {
    GEM_BUG_ON!(unsafe { (*ct).enabled });
    unsafe {
        tasklet_kill(ptr::addr_of_mut!((*ct).receive_tasklet));
        i915_vma_unpin_and_release(ptr::addr_of_mut!((*ct).vma), I915_VMA_RELEASE_MAP);
        ptr::write_bytes(ct.cast::<u8>(), 0, size_of::<IntelGucCt>());
    }
}

// upstream: intel_guc_ct.c intel_guc_ct_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_enable(ct: *mut IntelGucCt) -> i32 {
    let guc = unsafe { ct_to_guc(ct) };
    GEM_BUG_ON!(unsafe { (*ct).enabled });
    GEM_BUG_ON!(unsafe { (*ct).vma.is_null() });
    let obj = unsafe { (*(*ct).vma).obj };
    GEM_BUG_ON!(!unsafe { i915_gem_object_has_pinned_pages(obj) });
    let base = unsafe { crate::intel_guc_upstream::intel_guc_ggtt_offset(guc, (*ct).vma) };
    let blob = unsafe { (*ct).ctbs.send.desc.cast::<u8>() };
    GEM_BUG_ON!(blob.is_null());
    GEM_BUG_ON!(blob.cast::<GucCtBufferDesc>() != unsafe { (*ct).ctbs.send.desc });
    unsafe {
        guc_ct_buffer_reset(ptr::addr_of_mut!((*ct).ctbs.send));
        guc_ct_buffer_reset(ptr::addr_of_mut!((*ct).ctbs.recv));
    }

    let recv_desc = unsafe { (*ct).ctbs.recv.desc.cast::<u8>().offset_from(blob) as u32 };
    let recv_cmds = unsafe { (*ct).ctbs.recv.cmds.cast::<u8>().offset_from(blob) as u32 };
    let recv_size = unsafe { (*ct).ctbs.recv.size * 4 };
    let mut err =
        unsafe { ct_register_buffer(ct, false, base + recv_desc, base + recv_cmds, recv_size) };
    if err != 0 {
        return unsafe { ct_enable_error(ct, err) };
    }

    let send_desc = unsafe { (*ct).ctbs.send.desc.cast::<u8>().offset_from(blob) as u32 };
    let send_cmds = unsafe { (*ct).ctbs.send.cmds.cast::<u8>().offset_from(blob) as u32 };
    let send_size = unsafe { (*ct).ctbs.send.size * 4 };
    err = unsafe { ct_register_buffer(ct, true, base + send_desc, base + send_cmds, send_size) };
    if err != 0 {
        return unsafe { ct_enable_error(ct, err) };
    }
    err = unsafe { ct_control_enable(ct, true) };
    if err != 0 {
        return unsafe { ct_enable_error(ct, err) };
    }
    unsafe {
        (*ct).enabled = true;
        (*ct).stall_time = i64::MAX;
    }
    0
}

unsafe fn ct_enable_error(ct: *mut IntelGucCt, err: i32) -> i32 {
    CT_PROBE_ERROR!(
        ct,
        "Failed to enable CTB (%pe)\n",
        crate::linux::config::ERR_PTR::<c_void>(err)
    );
    CT_DEAD!(ct, 0);
    err
}

// upstream: intel_guc_ct.c intel_guc_ct_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_disable(ct: *mut IntelGucCt) {
    let guc = unsafe { ct_to_guc(ct) };
    GEM_BUG_ON!(!unsafe { (*ct).enabled });
    unsafe { (*ct).enabled = false };
    if unsafe { intel_guc_is_fw_running(guc) } {
        let _ = unsafe { ct_control_enable(ct, false) };
    }
}

// upstream: intel_guc_ct.c ct_track_lost_and_found()
#[cfg(CONFIG_DRM_I915_DEBUG_GEM)]
unsafe fn ct_track_lost_and_found(ct: *mut IntelGucCt, fence: u32, action: u32) {
    // The configured layout has DEBUG_GEM disabled; the debug-only request
    // history storage is therefore absent from IntelGucCtRequests.
    let _ = (ct, fence, action);
}

// upstream: intel_guc_ct.c ct_get_next_fence()
unsafe fn ct_get_next_fence(ct: *mut IntelGucCt) -> u32 {
    unsafe {
        (*ct).requests.last_fence = (*ct).requests.last_fence.wrapping_add(1);
        (*ct).requests.last_fence as u32
    }
}

unsafe fn desc_head(desc: *const GucCtBufferDesc) -> u32 {
    let desc = desc_layout(desc.cast_mut());
    unsafe { ptr::read_volatile(ptr::addr_of!((*desc).head)) }
}
unsafe fn desc_tail(desc: *const GucCtBufferDesc) -> u32 {
    let desc = desc_layout(desc.cast_mut());
    unsafe { ptr::read_volatile(ptr::addr_of!((*desc).tail)) }
}
unsafe fn desc_status(desc: *const GucCtBufferDesc) -> u32 {
    let desc = desc_layout(desc.cast_mut());
    unsafe { ptr::read_volatile(ptr::addr_of!((*desc).status)) }
}
unsafe fn desc_set_status(desc: *mut GucCtBufferDesc, value: u32) {
    let desc = unsafe { desc_layout(desc) };
    unsafe { ptr::write_volatile(ptr::addr_of_mut!((*desc).status), value) };
}
unsafe fn desc_set_head(desc: *mut GucCtBufferDesc, value: u32) {
    let desc = unsafe { desc_layout(desc) };
    unsafe { ptr::write_volatile(ptr::addr_of_mut!((*desc).head), value) };
}
unsafe fn desc_set_tail(desc: *mut GucCtBufferDesc, value: u32) {
    let desc = unsafe { desc_layout(desc) };
    unsafe { ptr::write_volatile(ptr::addr_of_mut!((*desc).tail), value) };
}

// upstream: intel_guc_ct.c ct_write()
unsafe fn ct_write(
    ct: *mut IntelGucCt,
    action: *const u32,
    len: u32,
    fence: u32,
    flags: u32,
) -> i32 {
    let ctb = unsafe { ptr::addr_of_mut!((*ct).ctbs.send) };
    let desc = unsafe { (*ctb).desc };
    let mut tail = unsafe { (*ctb).tail };
    let size = unsafe { (*ctb).size };
    let cmds = unsafe { (*ctb).cmds };
    if unsafe { desc_status(desc) } != crate::guc_ct::GUC_CTB_STATUS_NO_ERROR {
        return unsafe { ct_write_corrupt(ct, ctb) };
    }
    GEM_BUG_ON!(tail > size);

    let action0 = unsafe { ptr::read(action) };
    let header = REG_FIELD_PREP(GUC_CTB_MSG_0_FORMAT, GUC_CTB_FORMAT_HXG)
        | REG_FIELD_PREP(GUC_CTB_MSG_0_NUM_DWORDS, len)
        | REG_FIELD_PREP(GUC_CTB_MSG_0_FENCE, fence);
    let hxg_type = if flags & INTEL_GUC_CT_SEND_NB != 0 {
        crate::guc_ct::HXG_TYPE_FAST_REQUEST
    } else {
        crate::guc_ct::HXG_TYPE_REQUEST
    };
    let hxg = REG_FIELD_PREP(GUC_HXG_MSG_0_TYPE, hxg_type)
        | REG_FIELD_PREP(
            crate::guc_ct::GUC_HXG_REQUEST_MSG_0_ACTION
                | crate::guc_ct::GUC_HXG_REQUEST_MSG_0_DATA0,
            action0,
        );
    unsafe {
        ptr::write(cmds.add(tail as usize), header);
        tail = (tail + 1) % size;
        ptr::write(cmds.add(tail as usize), hxg);
        tail = (tail + 1) % size;
        for index in 1..len {
            ptr::write(
                cmds.add(tail as usize),
                ptr::read(action.add(index as usize)),
            );
            tail = (tail + 1) % size;
        }
    }
    GEM_BUG_ON!(tail > size);
    #[cfg(CONFIG_DRM_I915_DEBUG_GEM)]
    unsafe {
        ct_track_lost_and_found(
            ct,
            fence,
            REG_FIELD_GET(GUC_HXG_EVENT_MSG_0_ACTION, action0),
        )
    };

    unsafe { intel_guc_write_barrier(ct_to_guc(ct)) };
    unsafe {
        (*ctb).tail = tail;
        GEM_BUG_ON!(atomic_read(&(*ctb).space) < (len + GUC_CTB_HDR_LEN) as i32);
        atomic_add(-((len + GUC_CTB_HDR_LEN) as i32), &mut (*ctb).space);
    }
    unsafe { desc_set_tail(desc, tail) };
    0
}

unsafe fn ct_write_corrupt(ct: *mut IntelGucCt, ctb: *mut IntelGucCtBuffer) -> i32 {
    CT_ERROR!(
        ct,
        "Corrupted descriptor head=%u tail=%u status=%#x\n",
        unsafe { desc_head((*ctb).desc) },
        unsafe { desc_tail((*ctb).desc) },
        unsafe { desc_status((*ctb).desc) }
    );
    CT_DEAD!(ct, 0);
    unsafe { (*ctb).broken = true };
    -EPIPE
}

// upstream: intel_guc_ct.c wait_for_ct_request_update()
unsafe fn wait_for_ct_request_update(
    ct: *mut IntelGucCt,
    request: *mut CtRequest,
    status: *mut u32,
) -> i32 {
    let mut ct_enabled = true;
    let mut done = || {
        ct_enabled = unsafe { (*ct).enabled };
        !ct_enabled
            || REG_FIELD_GET(crate::guc_ct::GUC_HXG_MSG_0_ORIGIN, unsafe {
                ptr::read_volatile(ptr::addr_of!((*request).status))
            }) == GUC_HXG_ORIGIN_GUC
    };
    let mut timed_out = wait_for_atomic_us!(done(), 10);
    if timed_out {
        timed_out = wait_for!(done(), GUC_CTB_RESPONSE_TIMEOUT_LONG_MS);
    }
    let mut err = if timed_out {
        -crate::linux::config::ETIMEDOUT
    } else {
        0
    };
    if !ct_enabled {
        err = -ENODEV;
    }
    unsafe { ptr::write(status, ptr::read_volatile(ptr::addr_of!((*request).status))) };
    err
}

fn ktime_ms_delta(end: i64, start: i64) -> i64 {
    end.saturating_sub(start) / 1_000_000
}

// upstream: intel_guc_ct.c ct_deadlocked()
unsafe fn ct_deadlocked(ct: *mut IntelGucCt) -> bool {
    let delta = ktime_ms_delta(ktime_get(), unsafe { (*ct).stall_time });
    if delta > GUC_CTB_TIMEOUT_MS {
        // Preserve the source register selection (both local aliases name the
        // SEND descriptor in this Linux 7.2.3 diagnostic path).
        let send = unsafe { (*ct).ctbs.send.desc };
        let recv = unsafe { (*ct).ctbs.send.desc };
        CT_ERROR!(
            ct,
            "Communication stalled for %lld ms, desc status=%#x,%#x\n",
            delta,
            unsafe { desc_status(send) },
            unsafe { desc_status(recv) }
        );
        CT_ERROR!(
            ct,
            "H2G Space: %u (Bytes)\n",
            atomic_read(unsafe { &(*ct).ctbs.send.space }) * 4
        );
        CT_ERROR!(ct, "Head: %u (Dwords)\n", unsafe {
            desc_head((*ct).ctbs.send.desc)
        });
        CT_ERROR!(ct, "Tail: %u (Dwords)\n", unsafe {
            desc_tail((*ct).ctbs.send.desc)
        });
        CT_ERROR!(
            ct,
            "G2H Space: %u (Bytes)\n",
            atomic_read(unsafe { &(*ct).ctbs.recv.space }) * 4
        );
        CT_ERROR!(ct, "Head: %u\n (Dwords)", unsafe {
            desc_head((*ct).ctbs.recv.desc)
        });
        CT_ERROR!(ct, "Tail: %u\n (Dwords)", unsafe {
            desc_tail((*ct).ctbs.recv.desc)
        });
        CT_DEAD!(ct, 0);
        unsafe { (*ct).ctbs.send.broken = true };
        return true;
    }
    false
}

// upstream: intel_guc_ct.c g2h_has_room()
unsafe fn g2h_has_room(ct: *mut IntelGucCt, g2h_len_dw: u32) -> bool {
    let recv = unsafe { ptr::addr_of!((*ct).ctbs.recv) };
    g2h_len_dw == 0 || atomic_read(unsafe { &(*recv).space }) >= g2h_len_dw as i32
}

// upstream: intel_guc_ct.c g2h_reserve_space()
unsafe fn g2h_reserve_space(ct: *mut IntelGucCt, g2h_len_dw: u32) {
    lockdep_assert_held!(unsafe { &(*ct).ctbs.send.lock });
    GEM_BUG_ON!(!unsafe { g2h_has_room(ct, g2h_len_dw) });
    if g2h_len_dw != 0 {
        unsafe { atomic_add(-(g2h_len_dw as i32), &mut (*ct).ctbs.recv.space) };
    }
}

// upstream: intel_guc_ct.c g2h_release_space()
unsafe fn g2h_release_space(ct: *mut IntelGucCt, g2h_len_dw: u32) {
    unsafe { atomic_add(g2h_len_dw as i32, &mut (*ct).ctbs.recv.space) };
}

// upstream: intel_guc_ct.c h2g_has_room()
unsafe fn h2g_has_room(ct: *mut IntelGucCt, len_dw: u32) -> bool {
    let ctb = unsafe { ptr::addr_of_mut!((*ct).ctbs.send) };
    if atomic_read(unsafe { &(*ctb).space }) >= len_dw as i32 {
        return true;
    }
    let head = unsafe { desc_head((*ctb).desc) };
    let size = unsafe { (*ctb).size };
    if head > size {
        CT_ERROR!(ct, "Invalid head offset %u >= %u)\n", head, size);
        unsafe {
            desc_set_status(
                (*ctb).desc,
                desc_status((*ctb).desc) | GUC_CTB_STATUS_OVERFLOW,
            )
        };
        unsafe { (*ctb).broken = true };
        CT_DEAD!(ct, 0);
        return false;
    }
    let space = circ_space(unsafe { (*ctb).tail }, head, size);
    unsafe { atomic_set(&mut (*ctb).space, space as i32) };
    space >= len_dw
}

// upstream: intel_guc_ct.c has_room_nb()
unsafe fn has_room_nb(ct: *mut IntelGucCt, h2g_dw: u32, g2h_dw: u32) -> i32 {
    lockdep_assert_held!(unsafe { &(*ct).ctbs.send.lock });
    let h2g = unsafe { h2g_has_room(ct, h2g_dw) };
    let g2h = unsafe { g2h_has_room(ct, g2h_dw) };
    if !h2g || !g2h {
        if unsafe { (*ct).stall_time == i64::MAX } {
            unsafe { (*ct).stall_time = ktime_get() };
        }
        if !g2h {
            unsafe { tasklet_hi_schedule(ptr::addr_of_mut!((*ct).receive_tasklet)) };
        }
        if unsafe { ct_deadlocked(ct) } {
            return -EPIPE;
        }
        return -EBUSY;
    }
    unsafe { (*ct).stall_time = i64::MAX };
    0
}

// upstream: intel_guc_ct.c ct_send_nb()
unsafe fn ct_send_nb(ct: *mut IntelGucCt, action: *const u32, len: u32, flags: u32) -> i32 {
    let ctb = unsafe { ptr::addr_of_mut!((*ct).ctbs.send) };
    let g2h_len_dw = {
        let requested = REG_FIELD_GET(INTEL_GUC_CT_SEND_G2H_DW_MASK, flags);
        if requested != 0 {
            requested + GUC_CTB_HXG_MSG_MIN_LEN
        } else {
            0
        }
    };
    let mut irq_flags = 0;
    unsafe { spin_lock_irqsave(&mut (*ctb).lock, &mut irq_flags) };
    let mut ret = unsafe { has_room_nb(ct, len + GUC_CTB_HDR_LEN, g2h_len_dw) };
    if ret == 0 {
        let fence = unsafe { ct_get_next_fence(ct) };
        ret = unsafe { ct_write(ct, action, len, fence, flags) };
        if ret == 0 {
            unsafe {
                g2h_reserve_space(ct, g2h_len_dw);
                intel_guc_notify(ct_to_guc(ct));
            }
        }
    }
    unsafe { spin_unlock_irqrestore(&mut (*ctb).lock, irq_flags) };
    ret
}

// upstream: intel_guc_ct.c ct_send()
unsafe fn ct_send(
    ct: *mut IntelGucCt,
    action: *const u32,
    len: u32,
    response_buf: *mut u32,
    response_buf_size: u32,
    status: *mut u32,
) -> i32 {
    let ctb = unsafe { ptr::addr_of_mut!((*ct).ctbs.send) };
    let mut sleep_period_ms = 1u32;
    GEM_BUG_ON!(!unsafe { (*ct).enabled });
    GEM_BUG_ON!(len == 0);
    GEM_BUG_ON!(len > GUC_CTB_HXG_MSG_MAX_LEN - GUC_CTB_HDR_LEN);
    GEM_BUG_ON!(response_buf.is_null() && response_buf_size != 0);
    crate::linux::wait::might_sleep();

    'resend: loop {
        let mut flags = 0;
        'retry: loop {
            unsafe { spin_lock_irqsave(&mut (*ctb).lock, &mut flags) };
            if !unsafe { h2g_has_room(ct, len + GUC_CTB_HDR_LEN) }
                || !unsafe { g2h_has_room(ct, GUC_CTB_HXG_MSG_MAX_LEN) }
            {
                if unsafe { (*ct).stall_time == i64::MAX } {
                    unsafe { (*ct).stall_time = ktime_get() };
                }
                unsafe { spin_unlock_irqrestore(&mut (*ctb).lock, flags) };
                if unsafe { ct_deadlocked(ct) } {
                    return -EPIPE;
                }
                msleep(sleep_period_ms);
                sleep_period_ms <<= 1;
                continue 'retry;
            }
            unsafe { (*ct).stall_time = i64::MAX };

            let mut request = CtRequest {
                link: ListHead {
                    next: ptr::null_mut(),
                    prev: ptr::null_mut(),
                },
                fence: unsafe { ct_get_next_fence(ct) },
                status: 0,
                response_len: response_buf_size,
                response_buf,
            };
            unsafe {
                spin_lock(&mut (*ct).requests.lock);
                crate::linux_list::list_add_tail(
                    ptr::addr_of_mut!(request.link),
                    ptr::addr_of_mut!((*ct).requests.pending),
                );
                spin_unlock(&mut (*ct).requests.lock);
            }

            let mut err = unsafe { ct_write(ct, action, len, request.fence, 0) };
            unsafe {
                g2h_reserve_space(ct, GUC_CTB_HXG_MSG_MAX_LEN);
                spin_unlock_irqrestore(&mut (*ctb).lock, flags);
            }
            if err != 0 {
                unsafe { ct_unlink_request(ct, &mut request) };
                return err;
            }
            unsafe { intel_guc_notify(ct_to_guc(ct)) };
            err = unsafe { wait_for_ct_request_update(ct, &mut request, status) };
            unsafe { g2h_release_space(ct, GUC_CTB_HXG_MSG_MAX_LEN) };
            if err != 0 {
                if err == -ENODEV {
                    CT_DEBUG!(
                        ct,
                        "Request %#x (fence %u) cancelled as CTB is disabled\n",
                        unsafe { ptr::read(action) },
                        request.fence
                    );
                } else {
                    CT_ERROR!(
                        ct,
                        "No response for request %#x (fence %u)\n",
                        unsafe { ptr::read(action) },
                        request.fence
                    );
                }
                unsafe { ct_unlink_request(ct, &mut request) };
                return err;
            }

            let response_type = REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, unsafe { ptr::read(status) });
            let send_again = response_type == GUC_HXG_TYPE_NO_RESPONSE_RETRY;
            if send_again {
                CT_DEBUG!(
                    ct,
                    "retrying request %#x (%u)\n",
                    unsafe { ptr::read(action) },
                    REG_FIELD_GET(crate::guc_ct::GUC_HXG_RETRY_MSG_0_REASON, unsafe {
                        ptr::read(status)
                    })
                );
            } else if response_type != GUC_HXG_TYPE_RESPONSE_SUCCESS {
                err = -EIO;
            } else if !response_buf.is_null() {
                WARN_ON!(REG_FIELD_GET(GUC_HXG_RESPONSE_MSG_0_DATA0, request.status) != 0);
                err = request.response_len as i32;
            } else {
                WARN_ON!(request.response_len != 0);
                err = REG_FIELD_GET(GUC_HXG_RESPONSE_MSG_0_DATA0, unsafe { ptr::read(status) })
                    as i32;
            }
            unsafe { ct_unlink_request(ct, &mut request) };
            if send_again {
                continue 'resend;
            }
            return err;
        }
    }
}

unsafe fn ct_unlink_request(ct: *mut IntelGucCt, request: *mut CtRequest) {
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*ct).requests.lock, &mut flags) };
    unsafe {
        crate::linux_list::list_del_init(ptr::addr_of_mut!((*request).link));
        spin_unlock_irqrestore(&mut (*ct).requests.lock, flags);
    }
}

// upstream: intel_guc_ct.c intel_guc_ct_send()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_send(
    ct: *mut IntelGucCt,
    action: *const u32,
    len: u32,
    response_buf: *mut u32,
    response_buf_size: u32,
    flags: u32,
) -> i32 {
    let guc = unsafe { ct_to_guc(ct) };
    if !unsafe { (*ct).enabled } {
        let uc = unsafe { container_of!(guc, IntelUc, guc) };
        WARN_ON!(!unsafe { (*uc).reset_in_progress });
        return -ENODEV;
    }
    if unsafe { (*ct).ctbs.send.broken } {
        return -EPIPE;
    }
    if flags & INTEL_GUC_CT_SEND_NB != 0 {
        return unsafe { ct_send_nb(ct, action, len, flags) };
    }
    let mut status = u32::MAX;
    let ret = unsafe {
        ct_send(
            ct,
            action,
            len,
            response_buf,
            response_buf_size,
            &mut status,
        )
    };
    if ret < 0 {
        if ret != -ENODEV {
            CT_ERROR!(
                ct,
                "Sending action %#x failed (%pe) status=%#X\n",
                unsafe { ptr::read(action) },
                crate::linux::config::ERR_PTR::<c_void>(ret),
                status
            );
        }
    } else if ret != 0 {
        CT_DEBUG!(
            ct,
            "send action %#x returned %d (%#x)\n",
            unsafe { ptr::read(action) },
            ret,
            ret as u32
        );
    }
    ret
}

// upstream: intel_guc_ct.c ct_alloc_msg()
unsafe fn ct_alloc_msg(num_dwords: u32) -> *mut CtIncomingMsg {
    let bytes = size_of::<CtIncomingMsg>() + num_dwords as usize * size_of::<u32>();
    let msg = kmalloc(bytes, GFP_ATOMIC).cast::<CtIncomingMsg>();
    if !msg.is_null() {
        unsafe { (*msg).size = num_dwords };
    }
    msg
}

// upstream: intel_guc_ct.c ct_free_msg()
unsafe fn ct_free_msg(msg: *mut CtIncomingMsg) {
    unsafe { kfree(msg.cast::<c_void>()) };
}

// upstream: intel_guc_ct.c ct_read()
unsafe fn ct_read(ct: *mut IntelGucCt, out_msg: *mut *mut CtIncomingMsg) -> i32 {
    let ctb = unsafe { ptr::addr_of_mut!((*ct).ctbs.recv) };
    let desc = unsafe { (*ctb).desc };
    let mut head = unsafe { (*ctb).head };
    let tail = unsafe { desc_tail(desc) };
    let size = unsafe { (*ctb).size };
    let cmds = unsafe { (*ctb).cmds };
    if unsafe { (*ctb).broken } {
        return -EPIPE;
    }
    let mut status = unsafe { desc_status(desc) };
    if status & GUC_CTB_STATUS_UNUSED != 0 {
        CT_ERROR!(ct, "Unexpected G2H after GuC has stopped!\n");
        status &= !GUC_CTB_STATUS_UNUSED;
    }
    if status != crate::guc_ct::GUC_CTB_STATUS_NO_ERROR {
        return unsafe { ct_read_corrupt(ct, ctb) };
    }
    GEM_BUG_ON!(head > size);
    if tail >= size {
        CT_ERROR!(ct, "Invalid tail offset %u >= %u)\n", tail, size);
        unsafe { desc_set_status(desc, desc_status(desc) | GUC_CTB_STATUS_OVERFLOW) };
        return unsafe { ct_read_corrupt(ct, ctb) };
    }

    let mut available = tail as i32 - head as i32;
    if available == 0 {
        unsafe { ptr::write(out_msg, ptr::null_mut()) };
        return 0;
    }
    if available < 0 {
        available += size as i32;
    }
    GEM_BUG_ON!(available < 0);
    let header = unsafe { ptr::read(cmds.add(head as usize)) };
    head = (head + 1) % size;
    let len = REG_FIELD_GET(GUC_CTB_MSG_0_NUM_DWORDS, header) + GUC_CTB_MSG_MIN_LEN;
    if len > available as u32 {
        CT_ERROR!(
            ct,
            "Incomplete message header=%#x available=%d\n",
            header,
            available
        );
        unsafe { desc_set_status(desc, desc_status(desc) | GUC_CTB_STATUS_UNDERFLOW) };
        return unsafe { ct_read_corrupt(ct, ctb) };
    }

    let msg = unsafe { ct_alloc_msg(len) };
    if msg.is_null() {
        CT_ERROR!(ct, "No memory for CT message (%u dwords)\n", len);
        return available;
    }
    unsafe {
        ptr::write(ct_msg_data(msg), header);
        for index in 1..len {
            ptr::write(ct_msg_data(msg).add(index as usize), ptr::read(cmds.add(head as usize)));
            head = (head + 1) % size;
        }
        (*ctb).head = head;
        desc_set_head(desc, head);
        intel_guc_write_barrier(ct_to_guc(ct));
        ptr::write(out_msg, msg);
    }
    available - len as i32
}

unsafe fn ct_read_corrupt(ct: *mut IntelGucCt, ctb: *mut IntelGucCtBuffer) -> i32 {
    CT_ERROR!(
        ct,
        "Corrupted descriptor head=%u tail=%u status=%#x\n",
        unsafe { desc_head((*ctb).desc) },
        unsafe { desc_tail((*ctb).desc) },
        unsafe { desc_status((*ctb).desc) }
    );
    unsafe { (*ctb).broken = true };
    CT_DEAD!(ct, 0);
    -EPIPE
}

// upstream: intel_guc_ct.c ct_check_lost_and_found()
#[cfg(CONFIG_DRM_I915_DEBUG_GEM)]
unsafe fn ct_check_lost_and_found(ct: *mut IntelGucCt, fence: u32) -> bool {
    // The debug-only lost-and-found ring is intentionally not part of the
    // checked-in LinuxKPI layout (CONFIG_DRM_I915_DEBUG_GEM=false).
    let _ = (ct, fence);
    false
}

// upstream: intel_guc_ct.c ct_check_lost_and_found()
#[cfg(not(CONFIG_DRM_I915_DEBUG_GEM))]
unsafe fn ct_check_lost_and_found(_ct: *mut IntelGucCt, _fence: u32) -> bool {
    false
}

// upstream: intel_guc_ct.c ct_handle_response()
unsafe fn ct_handle_response(ct: *mut IntelGucCt, response: *mut CtIncomingMsg) -> i32 {
    let msg = unsafe { ct_msg_data(response) };
    let header = unsafe { ptr::read(msg) };
    let len = REG_FIELD_GET(GUC_CTB_MSG_0_NUM_DWORDS, header);
    let fence = REG_FIELD_GET(GUC_CTB_MSG_0_FENCE, header);
    let hxg = unsafe { msg.add(GUC_CTB_MSG_MIN_LEN as usize) };
    let hxg0 = unsafe { ptr::read(hxg) };
    let data = unsafe { hxg.add(GUC_HXG_MSG_MIN_LEN as usize) };
    let mut datalen = len.saturating_sub(GUC_HXG_MSG_MIN_LEN);
    GEM_BUG_ON!(len < GUC_HXG_MSG_MIN_LEN);
    GEM_BUG_ON!(REG_FIELD_GET(GUC_HXG_MSG_0_ORIGIN, hxg0) != GUC_HXG_ORIGIN_GUC);
    let response_type = REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, hxg0);
    GEM_BUG_ON!(
        response_type != GUC_HXG_TYPE_RESPONSE_SUCCESS
            && response_type != GUC_HXG_TYPE_NO_RESPONSE_RETRY
            && response_type != GUC_HXG_TYPE_RESPONSE_FAILURE
    );

    let requests = unsafe { ptr::addr_of_mut!((*ct).requests) };
    let pending = unsafe { ptr::addr_of_mut!((*requests).pending) };
    let mut flags = 0;
    let mut found = false;
    let mut err = 0;
    unsafe { spin_lock_irqsave(&mut (*requests).lock, &mut flags) };
    let mut node = unsafe { (*pending).next };
    while node != pending {
        let request = container_of!(node, CtRequest, link);
        if unsafe { (*request).fence } != fence {
            node = unsafe { (*node).next };
            continue;
        }
        let response_len = unsafe { (*request).response_len };
        if datalen > response_len {
            CT_ERROR!(
                ct,
                "Response %u too long (datalen %u > %u)\n",
                fence,
                datalen,
                response_len
            );
            datalen = datalen.min(response_len);
            err = -EMSGSIZE;
        }
        if datalen != 0 {
            let response_buf = unsafe { (*request).response_buf };
            if !response_buf.is_null() {
                unsafe { ptr::copy_nonoverlapping(data, response_buf, datalen as usize) };
            } else {
                err = -EMSGSIZE;
            }
        }
        unsafe {
            (*request).response_len = datalen;
            ptr::write_volatile(ptr::addr_of_mut!((*request).status), hxg0);
        }
        found = true;
        break;
    }

    if !found {
        CT_ERROR!(
            ct,
            "Unsolicited response message: len %u, data %#x (fence %u, last %u)\n",
            len,
            hxg0,
            fence,
            unsafe { (*requests).last_fence }
        );
        if !unsafe { ct_check_lost_and_found(ct, fence) } {
            let mut node = unsafe { (*pending).next };
            while node != pending {
                let request = container_of!(node, CtRequest, link);
                CT_ERROR!(ct, "request %u awaits response\n", unsafe {
                    (*request).fence
                });
                node = unsafe { (*node).next };
            }
        }
        err = -ENOKEY_LOCAL;
    }
    unsafe { spin_unlock_irqrestore(&mut (*requests).lock, flags) };

    if err != 0 {
        return err;
    }
    unsafe { ct_free_msg(response) };
    0
}

// upstream: intel_guc_ct.c ct_process_request()
unsafe fn ct_process_request(ct: *mut IntelGucCt, request: *mut CtIncomingMsg) -> i32 {
    let guc = unsafe { ct_to_guc(ct) };
    let msg = unsafe { ct_msg_data(request) };
    let hxg = unsafe { msg.add(GUC_CTB_MSG_MIN_LEN as usize) };
    let hxg_len = unsafe { (*request).size }.saturating_sub(GUC_CTB_MSG_MIN_LEN);
    let payload = unsafe { hxg.add(GUC_HXG_MSG_MIN_LEN as usize) };
    let action = REG_FIELD_GET(GUC_HXG_EVENT_MSG_0_ACTION, unsafe { ptr::read(hxg) });
    let len = hxg_len.saturating_sub(GUC_HXG_MSG_MIN_LEN);
    let ret = match action {
        INTEL_GUC_ACTION_DEFAULT => unsafe {
            intel_guc_to_host_process_recv_msg(guc, payload, len)
        },
        INTEL_GUC_ACTION_DEREGISTER_CONTEXT_DONE => unsafe {
            intel_guc_deregister_done_process_msg(guc, payload, len)
        },
        INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_DONE => unsafe {
            intel_guc_sched_done_process_msg(guc, payload, len)
        },
        INTEL_GUC_ACTION_CONTEXT_RESET_NOTIFICATION => unsafe {
            intel_guc_context_reset_process_msg(guc, payload, len)
        },
        INTEL_GUC_ACTION_STATE_CAPTURE_NOTIFICATION => unsafe {
            intel_guc_error_capture_process_msg(guc, payload, len)
        },
        INTEL_GUC_ACTION_ENGINE_FAILURE_NOTIFICATION => unsafe {
            intel_guc_engine_failure_process_msg(guc, payload, len)
        },
        INTEL_GUC_ACTION_NOTIFY_FLUSH_LOG_BUFFER_TO_FILE => {
            unsafe { intel_guc_log_handle_flush_event(ptr::addr_of_mut!((*guc).log)) };
            0
        }
        INTEL_GUC_ACTION_NOTIFY_CRASH_DUMP_POSTED | INTEL_GUC_ACTION_NOTIFY_EXCEPTION => unsafe {
            intel_guc_crash_process_msg(guc, action)
        },
        INTEL_GUC_ACTION_TLB_INVALIDATION_DONE => unsafe {
            intel_guc_tlb_invalidation_done(guc, payload, len)
        },
        _ => -EOPNOTSUPP_LOCAL,
    };
    if ret != 0 {
        CT_ERROR!(
            ct,
            "Failed to process request %04x (%pe)\n",
            action,
            crate::linux::config::ERR_PTR::<c_void>(ret)
        );
        return ret;
    }
    unsafe { ct_free_msg(request) };
    0
}

// upstream: intel_guc_ct.c ct_process_incoming_requests()
unsafe fn ct_process_incoming_requests(ct: *mut IntelGucCt) -> bool {
    let requests = unsafe { ptr::addr_of_mut!((*ct).requests) };
    let incoming = unsafe { ptr::addr_of_mut!((*requests).incoming) };
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*requests).lock, &mut flags) };
    let node = unsafe { (*incoming).next };
    let request = if node == incoming {
        ptr::null_mut()
    } else {
        let request = container_of!(node, CtIncomingMsg, link);
        unsafe { crate::linux_list::list_del(node) };
        request
    };
    let done = unsafe { crate::linux_list::list_empty(&*incoming) };
    unsafe { spin_unlock_irqrestore(&mut (*requests).lock, flags) };
    if request.is_null() {
        return true;
    }
    let err = unsafe { ct_process_request(ct, request) };
    if err != 0 {
        CT_ERROR!(
            ct,
            "Failed to process CT message (%pe)\n",
            crate::linux::config::ERR_PTR::<c_void>(err)
        );
        CT_DEAD!(ct, 0);
        unsafe { ct_free_msg(request) };
    }
    done
}

// upstream: intel_guc_ct.c ct_incoming_request_worker_func()
unsafe extern "C" fn ct_incoming_request_worker_func(work: *mut WorkStruct) {
    let requests = container_of!(
        work,
        crate::intel_guc_ct_types_upstream::IntelGucCtRequests,
        worker
    );
    let ct = container_of!(requests, IntelGucCt, requests);
    while !unsafe { ct_process_incoming_requests(ct) } {}
}

// upstream: intel_guc_ct.c ct_handle_event()
unsafe fn ct_handle_event(ct: *mut IntelGucCt, request: *mut CtIncomingMsg) -> i32 {
    let hxg = unsafe { ct_msg_data(request).add(GUC_CTB_MSG_MIN_LEN as usize) };
    let hxg0 = unsafe { ptr::read(hxg) };
    GEM_BUG_ON!(REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, hxg0) != GUC_HXG_TYPE_EVENT);
    let action = REG_FIELD_GET(GUC_HXG_EVENT_MSG_0_ACTION, hxg0);
    match action {
        INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_DONE
        | INTEL_GUC_ACTION_DEREGISTER_CONTEXT_DONE
        | INTEL_GUC_ACTION_TLB_INVALIDATION_DONE => unsafe {
            g2h_release_space(ct, (*request).size)
        },
        _ => {}
    }
    if action == INTEL_GUC_ACTION_TLB_INVALIDATION_DONE {
        return unsafe { ct_process_request(ct, request) };
    }
    let requests = unsafe { ptr::addr_of_mut!((*ct).requests) };
    let mut flags = 0;
    unsafe {
        spin_lock_irqsave(&mut (*requests).lock, &mut flags);
        crate::linux_list::list_add_tail(
            ptr::addr_of_mut!((*request).link),
            ptr::addr_of_mut!((*requests).incoming),
        );
        spin_unlock_irqrestore(&mut (*requests).lock, flags);
        queue_work(system_dfl_wq, ptr::addr_of_mut!((*requests).worker));
    }
    0
}

// upstream: intel_guc_ct.c ct_handle_hxg()
unsafe fn ct_handle_hxg(ct: *mut IntelGucCt, msg: *mut CtIncomingMsg) -> i32 {
    if unsafe { (*msg).size } < GUC_CTB_HXG_MSG_MIN_LEN {
        return -EBADMSG;
    }
    let hxg = unsafe { ct_msg_data(msg).add(GUC_CTB_MSG_MIN_LEN as usize) };
    let hxg0 = unsafe { ptr::read(hxg) };
    let origin = REG_FIELD_GET(GUC_HXG_MSG_0_ORIGIN, hxg0);
    if origin != GUC_HXG_ORIGIN_GUC {
        return -EPROTO;
    }
    match REG_FIELD_GET(GUC_HXG_MSG_0_TYPE, hxg0) {
        GUC_HXG_TYPE_EVENT => unsafe { ct_handle_event(ct, msg) },
        GUC_HXG_TYPE_RESPONSE_SUCCESS
        | GUC_HXG_TYPE_RESPONSE_FAILURE
        | GUC_HXG_TYPE_NO_RESPONSE_RETRY => unsafe { ct_handle_response(ct, msg) },
        _ => -EOPNOTSUPP_LOCAL,
    }
}

// upstream: intel_guc_ct.c ct_handle_msg()
unsafe fn ct_handle_msg(ct: *mut IntelGucCt, msg: *mut CtIncomingMsg) {
    let header = unsafe { ptr::read(ct_msg_data(msg)) };
    let format = REG_FIELD_GET(GUC_CTB_MSG_0_FORMAT, header);
    let err = if format == GUC_CTB_FORMAT_HXG {
        unsafe { ct_handle_hxg(ct, msg) }
    } else {
        -EOPNOTSUPP_LOCAL
    };
    if err != 0 {
        CT_ERROR!(
            ct,
            "Failed to process CT message (%pe)\n",
            crate::linux::config::ERR_PTR::<c_void>(err)
        );
        unsafe { ct_free_msg(msg) };
    }
}

// upstream: intel_guc_ct.c ct_receive()
unsafe fn ct_receive(ct: *mut IntelGucCt) -> i32 {
    let recv = unsafe { ptr::addr_of_mut!((*ct).ctbs.recv) };
    let mut msg = ptr::null_mut();
    let mut flags = 0;
    unsafe { spin_lock_irqsave(&mut (*recv).lock, &mut flags) };
    let ret = unsafe { ct_read(ct, &mut msg) };
    unsafe { spin_unlock_irqrestore(&mut (*recv).lock, flags) };
    if ret < 0 {
        return ret;
    }
    if !msg.is_null() {
        unsafe { ct_handle_msg(ct, msg) };
    }
    ret
}

// upstream: intel_guc_ct.c ct_try_receive_message()
unsafe fn ct_try_receive_message(ct: *mut IntelGucCt) {
    let guc = unsafe { ct_to_guc(ct) };
    if !unsafe { (*ct).enabled } {
        let gt = unsafe { guc_to_gt(guc) };
        let uc = unsafe { ptr::addr_of!((*gt).uc) };
        GEM_WARN_ON!(!unsafe { (*uc).reset_in_progress });
        return;
    }
    if !unsafe { (*guc).interrupts.enabled } {
        return;
    }
    if unsafe { ct_receive(ct) } > 0 {
        unsafe { tasklet_hi_schedule(ptr::addr_of_mut!((*ct).receive_tasklet)) };
    }
}

// upstream: intel_guc_ct.c ct_receive_tasklet_func()
unsafe extern "C" fn ct_receive_tasklet_func(tasklet: *mut TaskletStruct) {
    let ct = container_of!(tasklet, IntelGucCt, receive_tasklet);
    unsafe { ct_try_receive_message(ct) };
}

// upstream: intel_guc_ct.c intel_guc_ct_event_handler()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_event_handler(ct: *mut IntelGucCt) {
    if !unsafe { (*ct).enabled } {
        WARN_ON!(true);
        return;
    }
    unsafe { ct_try_receive_message(ct) };
}

// upstream: intel_guc_ct.c intel_guc_ct_print_info()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_guc_ct_print_info(ct: *mut IntelGucCt, printer: *mut DrmPrinter) {
    let enabled = unsafe { (*ct).enabled };
    drm_printer_write(
        printer,
        if enabled {
            "CT enabled\n"
        } else {
            "CT disabled\n"
        },
    );
    if !enabled {
        return;
    }
    let send = unsafe { ptr::addr_of!((*ct).ctbs.send) };
    let recv = unsafe { ptr::addr_of!((*ct).ctbs.recv) };
    let text = alloc::format!(
        "H2G Space: {}\nHead: {}\nTail: {}\nG2H Space: {}\nHead: {}\nTail: {}\n",
        atomic_read(unsafe { &(*send).space }) * 4,
        unsafe { desc_head((*send).desc) },
        unsafe { desc_tail((*send).desc) },
        atomic_read(unsafe { &(*recv).space }) * 4,
        unsafe { desc_head((*recv).desc) },
        unsafe { desc_tail((*recv).desc) },
    );
    drm_printer_write(printer, &text);
}

// upstream: intel_guc_ct.c ct_dead_ct_worker_func()
#[cfg(CONFIG_DRM_I915_DEBUG)]
unsafe extern "C" fn ct_dead_ct_worker_func(_work: *mut WorkStruct) {
    // CONFIG_DRM_I915_DEBUG is not enabled in the checked-in target layout.
}
