// SPDX-License-Identifier: MIT
// Copyright © 2014-2019 Intel Corporation.
// Source-order type and constant translation from Linux v7.2.3
// drivers/gpu/drm/i915/gt/uc/intel_guc_fwif.h.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use crate::intel_engine_types_upstream::{
    COMPUTE_CLASS, COPY_ENGINE_CLASS, MAX_ENGINE_CLASS, OTHER_CLASS, RENDER_CLASS,
    VIDEO_DECODE_CLASS, VIDEO_ENHANCEMENT_CLASS,
};

// Linux bitfield helpers used by this ABI header's macros.
#[inline]
pub const fn BIT(bit: u32) -> u32 {
    1u32 << bit
}

#[inline]
pub const fn GENMASK(high: u32, low: u32) -> u32 {
    (u32::MAX >> (31 - high)) << low
}

// Payload length only i.e. don't include G2H header length.
pub const G2H_LEN_DW_SCHED_CONTEXT_MODE_SET: u32 = 2;
pub const G2H_LEN_DW_DEREGISTER_CONTEXT: u32 = 1;
pub const G2H_LEN_DW_INVALIDATE_TLB: u32 = 1;

pub const GUC_CONTEXT_DISABLE: u32 = 0;
pub const GUC_CONTEXT_ENABLE: u32 = 1;

pub const GUC_CLIENT_PRIORITY_KMD_HIGH: u32 = 0;
pub const GUC_CLIENT_PRIORITY_HIGH: u32 = 1;
pub const GUC_CLIENT_PRIORITY_KMD_NORMAL: u32 = 2;
pub const GUC_CLIENT_PRIORITY_NORMAL: u32 = 3;
pub const GUC_CLIENT_PRIORITY_NUM: u32 = 4;

pub const GUC_MAX_CONTEXT_ID: u32 = 65_535;
pub const GUC_INVALID_CONTEXT_ID: u32 = GUC_MAX_CONTEXT_ID;

pub const GUC_RENDER_CLASS: u32 = 0;
pub const GUC_VIDEO_CLASS: u32 = 1;
pub const GUC_VIDEOENHANCE_CLASS: u32 = 2;
pub const GUC_BLITTER_CLASS: u32 = 3;
pub const GUC_COMPUTE_CLASS: u32 = 4;
pub const GUC_GSC_OTHER_CLASS: u32 = 5;
pub const GUC_LAST_ENGINE_CLASS: u32 = GUC_GSC_OTHER_CLASS;
pub const GUC_MAX_ENGINE_CLASSES: usize = 16;
pub const GUC_MAX_INSTANCES_PER_CLASS: usize = 32;

pub const GUC_DOORBELL_INVALID: u32 = 256;

// Work queue item status and type fields.
pub const WQ_STATUS_ACTIVE: u32 = 1;
pub const WQ_STATUS_SUSPENDED: u32 = 2;
pub const WQ_STATUS_CMD_ERROR: u32 = 3;
pub const WQ_STATUS_ENGINE_ID_NOT_USED: u32 = 4;
pub const WQ_STATUS_SUSPENDED_FROM_RESET: u32 = 5;
pub const WQ_TYPE_BATCH_BUF: u32 = 0x1;
pub const WQ_TYPE_PSEUDO: u32 = 0x2;
pub const WQ_TYPE_INORDER: u32 = 0x3;
pub const WQ_TYPE_NOOP: u32 = 0x4;
pub const WQ_TYPE_MULTI_LRC: u32 = 0x5;
pub const WQ_TYPE_MASK: u32 = GENMASK(7, 0);
pub const WQ_LEN_MASK: u32 = GENMASK(26, 16);
pub const WQ_GUC_ID_MASK: u32 = GENMASK(15, 0);
pub const WQ_RING_TAIL_MASK: u32 = GENMASK(28, 18);

pub const GUC_STAGE_DESC_ATTR_ACTIVE: u32 = BIT(0);
pub const GUC_STAGE_DESC_ATTR_PENDING_DB: u32 = BIT(1);
pub const GUC_STAGE_DESC_ATTR_KERNEL: u32 = BIT(2);
pub const GUC_STAGE_DESC_ATTR_PREEMPT: u32 = BIT(3);
pub const GUC_STAGE_DESC_ATTR_RESET: u32 = BIT(4);
pub const GUC_STAGE_DESC_ATTR_WQLOCKED: u32 = BIT(5);
pub const GUC_STAGE_DESC_ATTR_PCH: u32 = BIT(6);
pub const GUC_STAGE_DESC_ATTR_TERMINATED: u32 = BIT(7);

pub const GUC_CTL_LOG_PARAMS: u32 = 0;
pub const GUC_LOG_VALID: u32 = BIT(0);
pub const GUC_LOG_NOTIFY_ON_HALF_FULL: u32 = BIT(1);
pub const GUC_LOG_CAPTURE_ALLOC_UNITS: u32 = BIT(2);
pub const GUC_LOG_LOG_ALLOC_UNITS: u32 = BIT(3);
pub const GUC_LOG_CRASH_SHIFT: u32 = 4;
pub const GUC_LOG_CRASH_MASK: u32 = 0x3 << GUC_LOG_CRASH_SHIFT;
pub const GUC_LOG_DEBUG_SHIFT: u32 = 6;
pub const GUC_LOG_DEBUG_MASK: u32 = 0xF << GUC_LOG_DEBUG_SHIFT;
pub const GUC_LOG_CAPTURE_SHIFT: u32 = 10;
pub const GUC_LOG_CAPTURE_MASK: u32 = 0x3 << GUC_LOG_CAPTURE_SHIFT;
pub const GUC_LOG_BUF_ADDR_SHIFT: u32 = 12;

pub const GUC_CTL_WA: u32 = 1;
pub const GUC_WA_GAM_CREDITS: u32 = BIT(10);
pub const GUC_WA_DUAL_QUEUE: u32 = BIT(11);
pub const GUC_WA_RCS_RESET_BEFORE_RC6: u32 = BIT(13);
pub const GUC_WA_PRE_PARSER: u32 = BIT(14);
pub const GUC_WA_CONTEXT_ISOLATION: u32 = BIT(15);
pub const GUC_WA_RCS_CCS_SWITCHOUT: u32 = BIT(16);
pub const GUC_WA_HOLD_CCS_SWITCHOUT: u32 = BIT(17);
pub const GUC_WA_POLLCS: u32 = BIT(18);
pub const GUC_WA_RCS_REGS_IN_CCS_REGS_LIST: u32 = BIT(21);
pub const GUC_WA_ENABLE_TSC_CHECK_ON_RC6: u32 = BIT(22);

pub const GUC_CTL_FEATURE: u32 = 2;
pub const GUC_CTL_ENABLE_GUC_PXP_CTL: u32 = BIT(1);
pub const GUC_CTL_ENABLE_SLPC: u32 = BIT(2);
pub const GUC_CTL_DISABLE_SCHEDULER: u32 = BIT(14);

pub const GUC_CTL_DEBUG: u32 = 3;
pub const GUC_LOG_VERBOSITY_SHIFT: u32 = 0;
pub const GUC_LOG_VERBOSITY_LOW: u32 = 0 << GUC_LOG_VERBOSITY_SHIFT;
pub const GUC_LOG_VERBOSITY_MED: u32 = 1 << GUC_LOG_VERBOSITY_SHIFT;
pub const GUC_LOG_VERBOSITY_HIGH: u32 = 2 << GUC_LOG_VERBOSITY_SHIFT;
pub const GUC_LOG_VERBOSITY_ULTRA: u32 = 3 << GUC_LOG_VERBOSITY_SHIFT;
pub const GUC_LOG_VERBOSITY_MIN: u32 = 0;
pub const GUC_LOG_VERBOSITY_MAX: u32 = 3;
pub const GUC_LOG_VERBOSITY_MASK: u32 = 0x0000_000f;
pub const GUC_LOG_DESTINATION_MASK: u32 = 3 << 4;
pub const GUC_LOG_DISABLED: u32 = 1 << 6;
pub const GUC_PROFILE_ENABLED: u32 = 1 << 7;

pub const GUC_CTL_ADS: u32 = 4;
pub const GUC_ADS_ADDR_SHIFT: u32 = 1;
pub const GUC_ADS_ADDR_MASK: u32 = 0x000f_ffff << GUC_ADS_ADDR_SHIFT;
pub const GUC_CTL_DEVID: u32 = 5;
// Defined by intel_guc_reg.h, included by the upstream header users.
pub const SOFT_SCRATCH_COUNT: u32 = 16;
pub const GUC_CTL_MAX_DWORDS: u32 = SOFT_SCRATCH_COUNT - 2;

pub const GUC_GENERIC_GT_SYSINFO_SLICE_ENABLED: u32 = 0;
pub const GUC_GENERIC_GT_SYSINFO_VDBOX_SFC_SUPPORT_MASK: u32 = 1;
pub const GUC_GENERIC_GT_SYSINFO_DOORBELL_COUNT_PER_SQIDI: u32 = 2;
pub const GUC_GENERIC_GT_SYSINFO_MAX: usize = 16;

pub const GUC_ENGINE_CLASS_SHIFT: u32 = 0;
pub const GUC_ENGINE_CLASS_MASK: u32 = 0x7 << GUC_ENGINE_CLASS_SHIFT;
pub const GUC_ENGINE_INSTANCE_SHIFT: u32 = 3;
pub const GUC_ENGINE_INSTANCE_MASK: u32 = 0xf << GUC_ENGINE_INSTANCE_SHIFT;
pub const GUC_ENGINE_ALL_INSTANCES: u32 = BIT(7);

#[inline]
pub const fn MAKE_GUC_ID(class: u32, instance: u32) -> u32 {
    (class << GUC_ENGINE_CLASS_SHIFT) | (instance << GUC_ENGINE_INSTANCE_SHIFT)
}

#[inline]
pub const fn GUC_ID_TO_ENGINE_CLASS(guc_id: u32) -> u32 {
    (guc_id & GUC_ENGINE_CLASS_MASK) >> GUC_ENGINE_CLASS_SHIFT
}

#[inline]
pub const fn GUC_ID_TO_ENGINE_INSTANCE(guc_id: u32) -> u32 {
    (guc_id & GUC_ENGINE_INSTANCE_MASK) >> GUC_ENGINE_INSTANCE_SHIFT
}

// Source fields are HOST2GUC_PC_SLPC_REQUEST_MSG_1_EVENT_ID (0xff << 8) and
// HOST2GUC_PC_SLPC_REQUEST_MSG_1_EVENT_ARGC (0xff << 0), from the included
// guc_actions_slpc_abi.h.
#[inline]
pub const fn SLPC_EVENT(id: u32, c: u32) -> u32 {
    ((id & 0xff) << 8) | (c & 0xff)
}

// The two static maps in the C header, indexed by the shared engine class IDs.
pub const engine_class_guc_class_map: [u8; (MAX_ENGINE_CLASS as usize) + 1] = [
    GUC_RENDER_CLASS as u8,
    GUC_BLITTER_CLASS as u8,
    GUC_VIDEO_CLASS as u8,
    GUC_VIDEOENHANCE_CLASS as u8,
    GUC_GSC_OTHER_CLASS as u8,
    GUC_COMPUTE_CLASS as u8,
];
pub const guc_class_engine_class_map: [u8; (GUC_LAST_ENGINE_CLASS as usize) + 1] = [
    RENDER_CLASS as u8,
    VIDEO_DECODE_CLASS as u8,
    VIDEO_ENHANCEMENT_CLASS as u8,
    COPY_ENGINE_CLASS as u8,
    COMPUTE_CLASS as u8,
    OTHER_CLASS as u8,
];

const _: [(); (MAX_ENGINE_CLASS as usize) + 1] = [(); engine_class_guc_class_map.len()];
const _: [(); (GUC_LAST_ENGINE_CLASS as usize) + 1] = [(); guc_class_engine_class_map.len()];

#[inline]
pub fn engine_class_to_guc_class(class: u8) -> u8 {
    GEM_BUG_ON!(class as i32 > MAX_ENGINE_CLASS);
    engine_class_guc_class_map[class as usize]
}

#[inline]
pub fn guc_class_to_engine_class(guc_class: u8) -> u8 {
    GEM_BUG_ON!(guc_class as u32 > GUC_LAST_ENGINE_CLASS);
    guc_class_engine_class_map[guc_class as usize]
}

// Work item for submitting workloads into work queue of GuC.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_wq_item {
    pub header: u32,
    pub context_desc: u32,
    pub submit_element_info: u32,
    pub fence_id: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_process_desc_v69 {
    pub stage_id: u32,
    pub db_base_addr: u64,
    pub head: u32,
    pub tail: u32,
    pub error_offset: u32,
    pub wq_base_addr: u64,
    pub wq_size_bytes: u32,
    pub wq_status: u32,
    pub engine_presence: u32,
    pub priority: u32,
    pub reserved: [u32; 36],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_sched_wq_desc {
    pub head: u32,
    pub tail: u32,
    pub error_offset: u32,
    pub wq_status: u32,
    pub reserved: [u32; 28],
}

// Helper for context registration H2G.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct guc_ctxt_registration_info {
    pub flags: u32,
    pub context_idx: u32,
    pub engine_class: u32,
    pub engine_submit_mask: u32,
    pub wq_desc_lo: u32,
    pub wq_desc_hi: u32,
    pub wq_base_lo: u32,
    pub wq_base_hi: u32,
    pub wq_size: u32,
    pub hwlrca_lo: u32,
    pub hwlrca_hi: u32,
}
pub const CONTEXT_REGISTRATION_FLAG_KMD: u32 = BIT(0);
pub const CONTEXT_POLICY_FLAG_PREEMPT_TO_IDLE_V69: u32 = BIT(0);

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_lrc_desc_v69 {
    pub hw_context_desc: u32,
    pub slpm_perf_mode_hint: u32,
    pub slpm_freq_hint: u32,
    pub engine_submit_mask: u32,
    pub engine_class: u8,
    pub reserved0: [u8; 3],
    pub priority: u32,
    pub process_desc: u32,
    pub wq_addr: u32,
    pub wq_size: u32,
    pub context_flags: u32,
    pub execution_quantum: u32,
    pub preemption_timeout: u32,
    pub policy_flags: u32,
    pub reserved1: [u32; 19],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_klv_generic_dw_t {
    pub kl: u32,
    pub value: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_update_context_policy_header {
    pub action: u32,
    pub ctx_id: u32,
}

// Value from the included drivers/gpu/drm/i915/gt/uc/abi/guc_klvs_abi.h.
pub const GUC_CONTEXT_POLICIES_KLV_NUM_IDS: usize = 5;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_update_context_policy {
    pub header: guc_update_context_policy_header,
    pub klv: [guc_klv_generic_dw_t; GUC_CONTEXT_POLICIES_KLV_NUM_IDS],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_update_scheduling_policy_header {
    pub action: u32,
}

pub const MAX_SCHEDULING_POLICY_SIZE: usize = 3;

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_update_scheduling_policy {
    pub header: guc_update_scheduling_policy_header,
    pub data: [u32; MAX_SCHEDULING_POLICY_SIZE],
}

pub const GUC_POWER_UNSPECIFIED: u32 = 0;
pub const GUC_POWER_D0: u32 = 1;
pub const GUC_POWER_D1: u32 = 2;
pub const GUC_POWER_D2: u32 = 3;
pub const GUC_POWER_D3: u32 = 4;

pub const GLOBAL_SCHEDULE_POLICY_RC_YIELD_DURATION: u32 = 100;
pub const GLOBAL_SCHEDULE_POLICY_RC_YIELD_RATIO: u32 = 50;
pub const GLOBAL_POLICY_MAX_NUM_WI: u32 = 15;
pub const GLOBAL_POLICY_DISABLE_ENGINE_RESET: u32 = BIT(0);
pub const GLOBAL_POLICY_DEFAULT_DPC_PROMOTE_TIME_US: u32 = 500_000;
pub const GUC_POLICY_MAX_EXEC_QUANTUM_US: u64 = 100 * 1000 * 1000;
pub const GUC_POLICY_MAX_PREEMPT_TIMEOUT_US: u64 = 100 * 1000 * 1000;

pub const fn guc_policy_max_exec_quantum_ms() -> u32 {
    (GUC_POLICY_MAX_EXEC_QUANTUM_US / 1000) as u32
}

pub const fn guc_policy_max_preempt_timeout_ms() -> u32 {
    (GUC_POLICY_MAX_PREEMPT_TIMEOUT_US / 1000) as u32
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_policies {
    pub submission_queue_depth: [u32; GUC_MAX_ENGINE_CLASSES],
    pub dpc_promote_time: u32,
    pub is_valid: u32,
    pub max_num_work_items: u32,
    pub global_flags: u32,
    pub reserved: [u32; 4],
}

// GuC MMIO register state.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_mmio_reg {
    pub offset: u32,
    pub value: u32,
    pub flags: u32,
    pub mask: u32,
}
pub const GUC_REGSET_MASKED: u32 = BIT(0);
pub const GUC_REGSET_NEEDS_STEERING: u32 = BIT(1);
pub const GUC_REGSET_MASKED_WITH_VALUE: u32 = BIT(2);
pub const GUC_REGSET_RESTORE_ONLY: u32 = BIT(3);
pub const GUC_REGSET_STEERING_GROUP: u32 = GENMASK(15, 12);
pub const GUC_REGSET_STEERING_INSTANCE: u32 = GENMASK(23, 20);

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_mmio_reg_set {
    pub address: u32,
    pub count: u16,
    pub reserved: u16,
}

// HW info.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_gt_system_info {
    pub mapping_table: [[u8; GUC_MAX_INSTANCES_PER_CLASS]; GUC_MAX_ENGINE_CLASSES],
    pub engine_enabled_masks: [u32; GUC_MAX_ENGINE_CLASSES],
    pub generic_gt_sysinfo: [u32; GUC_GENERIC_GT_SYSINFO_MAX],
}

pub const GUC_CAPTURE_LIST_INDEX_PF: u32 = 0;
pub const GUC_CAPTURE_LIST_INDEX_VF: u32 = 1;
pub const GUC_CAPTURE_LIST_INDEX_MAX: usize = 2;

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum guc_capture_type {
    GUC_CAPTURE_LIST_TYPE_GLOBAL = 0,
    GUC_CAPTURE_LIST_TYPE_ENGINE_CLASS = 1,
    GUC_CAPTURE_LIST_TYPE_ENGINE_INSTANCE = 2,
    GUC_CAPTURE_LIST_TYPE_MAX = 3,
}

pub const GUC_CAPTURE_LIST_CLASS_RENDER_COMPUTE: u32 = 0;
pub const GUC_CAPTURE_LIST_CLASS_VIDEO: u32 = 1;
pub const GUC_CAPTURE_LIST_CLASS_VIDEOENHANCE: u32 = 2;
pub const GUC_CAPTURE_LIST_CLASS_BLITTER: u32 = 3;
pub const GUC_CAPTURE_LIST_CLASS_GSC_OTHER: u32 = 4;

// GuC Additional Data Struct.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_ads {
    pub reg_state_list: [[guc_mmio_reg_set; GUC_MAX_INSTANCES_PER_CLASS]; GUC_MAX_ENGINE_CLASSES],
    pub reserved0: u32,
    pub scheduler_policies: u32,
    pub gt_system_info: u32,
    pub reserved1: u32,
    pub control_data: u32,
    pub golden_context_lrca: [u32; GUC_MAX_ENGINE_CLASSES],
    pub eng_state_size: [u32; GUC_MAX_ENGINE_CLASSES],
    pub private_data: u32,
    pub reserved2: u32,
    pub capture_instance: [[u32; GUC_MAX_ENGINE_CLASSES]; GUC_CAPTURE_LIST_INDEX_MAX],
    pub capture_class: [[u32; GUC_MAX_ENGINE_CLASSES]; GUC_CAPTURE_LIST_INDEX_MAX],
    pub capture_global: [u32; GUC_CAPTURE_LIST_INDEX_MAX],
    pub wa_klv_addr_lo: u32,
    pub wa_klv_addr_hi: u32,
    pub wa_klv_size: u32,
    pub reserved: [u32; 11],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_engine_usage_record {
    pub current_context_index: u32,
    pub last_switch_in_stamp: u32,
    pub reserved0: u32,
    pub total_runtime: u32,
    pub reserved1: [u32; 4],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_engine_usage {
    pub engines: [[guc_engine_usage_record; GUC_MAX_INSTANCES_PER_CLASS]; GUC_MAX_ENGINE_CLASSES],
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum guc_log_buffer_type {
    GUC_DEBUG_LOG_BUFFER = 0,
    GUC_CRASH_DUMP_LOG_BUFFER = 1,
    GUC_CAPTURE_LOG_BUFFER = 2,
    GUC_MAX_LOG_BUFFER   = 3,
}

pub const GUC_LOG_BUFFER_FULL_CNT_SHIFT: u32 = 1;
pub const GUC_LOG_BUFFER_FULL_CNT_MASK: u32 = 0xF << GUC_LOG_BUFFER_FULL_CNT_SHIFT;
pub const GUC_LOG_BUFFER_RESERVED_MASK: u32 = 0xffff_ffe0;
pub const GUC_LOG_BUFFER_STATE_FLUSH_TO_FILE: u32 = BIT(0);

/// The C anonymous bitfield struct and its `u32 flags` member overlap in one
/// 32-bit word. Rust has no native C bitfields; this union preserves the exact
/// packed ABI storage and exposes the named bitfields through accessors.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub union GucLogBufferStateFlags {
    pub bitfield_storage: u32,
    pub flags: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct guc_log_buffer_state {
    pub marker: [u32; 2],
    pub read_ptr: u32,
    pub write_ptr: u32,
    pub size: u32,
    pub sampled_write_ptr: u32,
    pub wrap_offset: u32,
    pub flags: GucLogBufferStateFlags,
    pub version: u32,
}

impl guc_log_buffer_state {
    #[inline]
    pub fn flush_to_file(&self) -> u32 {
        let flags = unsafe { self.flags.flags };
        guc_log_buffer_flush_to_file(flags)
    }

    #[inline]
    pub fn buffer_full_cnt(&self) -> u32 {
        let flags = unsafe { self.flags.flags };
        guc_log_buffer_full_count(flags)
    }

    #[inline]
    pub fn set_flush_to_file(&mut self, value: u32) {
        let flags = unsafe { self.flags.flags };
        self.flags.flags = (flags & !GUC_LOG_BUFFER_STATE_FLUSH_TO_FILE)
            | (value & GUC_LOG_BUFFER_STATE_FLUSH_TO_FILE);
    }
}

#[inline]
pub const fn guc_log_buffer_flush_to_file(flags: u32) -> u32 {
    flags & GUC_LOG_BUFFER_STATE_FLUSH_TO_FILE
}

#[inline]
pub const fn guc_log_buffer_full_count(flags: u32) -> u32 {
    (flags & GUC_LOG_BUFFER_FULL_CNT_MASK) >> GUC_LOG_BUFFER_FULL_CNT_SHIFT
}

#[inline]
pub const fn guc_log_buffer_reserved(flags: u32) -> u32 {
    (flags & GUC_LOG_BUFFER_RESERVED_MASK) >> 5
}

pub const INTEL_GUC_RECV_MSG_CRASH_DUMP_POSTED: u32 = BIT(1);
pub const INTEL_GUC_RECV_MSG_EXCEPTION: u32 = BIT(30);

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_recv_message {
    INTEL_GUC_RECV_MSG_CRASH_DUMP_POSTED = INTEL_GUC_RECV_MSG_CRASH_DUMP_POSTED as i32,
    INTEL_GUC_RECV_MSG_EXCEPTION = INTEL_GUC_RECV_MSG_EXCEPTION as i32,
}

// Useful layout checks for the source-packed records above.
const _: [(); 16] = [(); core::mem::size_of::<guc_wq_item>()];
const _: [(); 192] = [(); core::mem::size_of::<guc_process_desc_v69>()];
const _: [(); 128] = [(); core::mem::size_of::<guc_sched_wq_desc>()];
const _: [(); 44] = [(); core::mem::size_of::<guc_ctxt_registration_info>()];
const _: [(); 128] = [(); core::mem::size_of::<guc_lrc_desc_v69>()];
const _: [(); 48] = [(); core::mem::size_of::<guc_update_context_policy>()];
const _: [(); 16] = [(); core::mem::size_of::<guc_update_scheduling_policy>()];
const _: [(); 96] = [(); core::mem::size_of::<guc_policies>()];
const _: [(); 16] = [(); core::mem::size_of::<guc_mmio_reg>()];
const _: [(); 8] = [(); core::mem::size_of::<guc_mmio_reg_set>()];
const _: [(); 640] = [(); core::mem::size_of::<guc_gt_system_info>()];
const _: [(); 4572] = [(); core::mem::size_of::<guc_ads>()];
const _: [(); 32] = [(); core::mem::size_of::<guc_engine_usage_record>()];
const _: [(); 16384] = [(); core::mem::size_of::<guc_engine_usage>()];
const _: [(); 36] = [(); core::mem::size_of::<guc_log_buffer_state>()];

// CamelCase aliases for Rust callers; C tag spellings remain available above.
pub type GucWqItem = guc_wq_item;
pub type GucProcessDescV69 = guc_process_desc_v69;
pub type GucSchedWqDesc = guc_sched_wq_desc;
pub type GucContextRegistrationInfo = guc_ctxt_registration_info;
pub type GucLrcDescV69 = guc_lrc_desc_v69;
pub type GucKlvGenericDw = guc_klv_generic_dw_t;
pub type GucUpdateContextPolicyHeader = guc_update_context_policy_header;
pub type GucUpdateContextPolicy = guc_update_context_policy;
pub type GucUpdateSchedulingPolicyHeader = guc_update_scheduling_policy_header;
pub type GucUpdateSchedulingPolicy = guc_update_scheduling_policy;
pub type GucPolicies = guc_policies;
pub type GucMmioReg = guc_mmio_reg;
pub type GucMmioRegSet = guc_mmio_reg_set;
pub type GucGtSystemInfo = guc_gt_system_info;
pub type GucAds = guc_ads;
pub type GucEngineUsageRecord = guc_engine_usage_record;
pub type GucEngineUsage = guc_engine_usage;
pub type GucLogBufferState = guc_log_buffer_state;
pub type GucCaptureType = guc_capture_type;
pub type GucLogBufferType = guc_log_buffer_type;
pub type IntelGucRecvMessage = intel_guc_recv_message;
