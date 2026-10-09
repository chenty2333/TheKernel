// SPDX-License-Identifier: MIT
// Copyright © 2014-2021 Intel Corporation.
// Source-order ABI constants and enum bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/uc/abi/guc_actions_abi.h.

#![allow(non_camel_case_types, non_snake_case)]

// Values used here are defined by the included guc_messages_abi.h. They are
// named privately to avoid claiming ownership of that separate ABI header.
const HXG_REQUEST_MSG_MIN_LEN: u32 = 1;
const HXG_REQUEST_MSG_0_DATA0: u32 = 0x0fff_0000;
const HXG_REQUEST_MSG_N_DATA_N: u32 = 0xffff_ffff;
const HXG_RESPONSE_MSG_MIN_LEN: u32 = 1;
const HXG_RESPONSE_MSG_0_DATA0: u32 = 0x0fff_ffff;

// REG_GENMASK/REG_BIT spellings in the source expand to these 32-bit values.
const fn reg_genmask(high: u32, low: u32) -> u32 {
    (u32::MAX >> (31 - high)) << low
}
const fn reg_bit(bit: u32) -> u32 {
    1u32 << bit
}

/// HOST2GUC_SELF_CFG request/response ABI.
pub const GUC_ACTION_HOST2GUC_SELF_CFG: u32 = 0x0508;
pub const HOST2GUC_SELF_CFG_REQUEST_MSG_LEN: u32 = HXG_REQUEST_MSG_MIN_LEN + 3;
pub const HOST2GUC_SELF_CFG_REQUEST_MSG_0_MBZ: u32 = HXG_REQUEST_MSG_0_DATA0;
pub const HOST2GUC_SELF_CFG_REQUEST_MSG_1_KLV_KEY: u32 = 0xffff_0000;
pub const HOST2GUC_SELF_CFG_REQUEST_MSG_1_KLV_LEN: u32 = 0x0000_ffff;
pub const HOST2GUC_SELF_CFG_REQUEST_MSG_2_VALUE32: u32 = HXG_REQUEST_MSG_N_DATA_N;
pub const HOST2GUC_SELF_CFG_REQUEST_MSG_3_VALUE64: u32 = HXG_REQUEST_MSG_N_DATA_N;
pub const HOST2GUC_SELF_CFG_RESPONSE_MSG_LEN: u32 = HXG_RESPONSE_MSG_MIN_LEN;
pub const HOST2GUC_SELF_CFG_RESPONSE_MSG_0_NUM: u32 = HXG_RESPONSE_MSG_0_DATA0;

/// HOST2GUC_CONTROL_CTB request/response ABI.
pub const GUC_ACTION_HOST2GUC_CONTROL_CTB: u32 = 0x4509;
pub const HOST2GUC_CONTROL_CTB_REQUEST_MSG_LEN: u32 = HXG_REQUEST_MSG_MIN_LEN + 1;
pub const HOST2GUC_CONTROL_CTB_REQUEST_MSG_0_MBZ: u32 = HXG_REQUEST_MSG_0_DATA0;
pub const HOST2GUC_CONTROL_CTB_REQUEST_MSG_1_CONTROL: u32 = HXG_REQUEST_MSG_N_DATA_N;
pub const GUC_CTB_CONTROL_DISABLE: u32 = 0;
pub const GUC_CTB_CONTROL_ENABLE: u32 = 1;
pub const HOST2GUC_CONTROL_CTB_RESPONSE_MSG_LEN: u32 = HXG_RESPONSE_MSG_MIN_LEN;
pub const HOST2GUC_CONTROL_CTB_RESPONSE_MSG_0_MBZ: u32 = HXG_RESPONSE_MSG_0_DATA0;

/// Legacy action identifiers.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_action {
    INTEL_GUC_ACTION_DEFAULT = 0x0,
    INTEL_GUC_ACTION_REQUEST_PREEMPTION = 0x2,
    INTEL_GUC_ACTION_REQUEST_ENGINE_RESET = 0x3,
    INTEL_GUC_ACTION_ALLOCATE_DOORBELL = 0x10,
    INTEL_GUC_ACTION_DEALLOCATE_DOORBELL = 0x20,
    INTEL_GUC_ACTION_LOG_BUFFER_FILE_FLUSH_COMPLETE = 0x30,
    INTEL_GUC_ACTION_UK_LOG_ENABLE_LOGGING = 0x40,
    INTEL_GUC_ACTION_FORCE_LOG_BUFFER_FLUSH = 0x302,
    INTEL_GUC_ACTION_ENTER_S_STATE = 0x501,
    INTEL_GUC_ACTION_EXIT_S_STATE = 0x502,
    INTEL_GUC_ACTION_GLOBAL_SCHED_POLICY_CHANGE = 0x506,
    INTEL_GUC_ACTION_UPDATE_SCHEDULING_POLICIES_KLV = 0x509,
    INTEL_GUC_ACTION_SCHED_CONTEXT = 0x1000,
    INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_SET = 0x1001,
    INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_DONE = 0x1002,
    INTEL_GUC_ACTION_SCHED_ENGINE_MODE_SET = 0x1003,
    INTEL_GUC_ACTION_SCHED_ENGINE_MODE_DONE = 0x1004,
    INTEL_GUC_ACTION_V69_SET_CONTEXT_PRIORITY = 0x1005,
    INTEL_GUC_ACTION_V69_SET_CONTEXT_EXECUTION_QUANTUM = 0x1006,
    INTEL_GUC_ACTION_V69_SET_CONTEXT_PREEMPTION_TIMEOUT = 0x1007,
    INTEL_GUC_ACTION_CONTEXT_RESET_NOTIFICATION = 0x1008,
    INTEL_GUC_ACTION_ENGINE_FAILURE_NOTIFICATION = 0x1009,
    INTEL_GUC_ACTION_HOST2GUC_UPDATE_CONTEXT_POLICIES = 0x100b,
    INTEL_GUC_ACTION_SETUP_PC_GUCRC = 0x3004,
    INTEL_GUC_ACTION_AUTHENTICATE_HUC = 0x4000,
    INTEL_GUC_ACTION_GET_HWCONFIG = 0x4100,
    INTEL_GUC_ACTION_REGISTER_CONTEXT = 0x4502,
    INTEL_GUC_ACTION_DEREGISTER_CONTEXT = 0x4503,
    INTEL_GUC_ACTION_DEREGISTER_CONTEXT_DONE = 0x4600,
    INTEL_GUC_ACTION_REGISTER_CONTEXT_MULTI_LRC = 0x4601,
    INTEL_GUC_ACTION_CLIENT_SOFT_RESET = 0x5507,
    INTEL_GUC_ACTION_SET_ENG_UTIL_BUFF = 0x550a,
    INTEL_GUC_ACTION_TLB_INVALIDATION = 0x7000,
    INTEL_GUC_ACTION_TLB_INVALIDATION_DONE = 0x7001,
    INTEL_GUC_ACTION_STATE_CAPTURE_NOTIFICATION = 0x8002,
    INTEL_GUC_ACTION_NOTIFY_FLUSH_LOG_BUFFER_TO_FILE = 0x8003,
    INTEL_GUC_ACTION_NOTIFY_CRASH_DUMP_POSTED = 0x8004,
    INTEL_GUC_ACTION_NOTIFY_EXCEPTION = 0x8005,
    INTEL_GUC_ACTION_LIMIT = 0x8006,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_rc_options {
    INTEL_GUCRC_HOST_CONTROL = 0,
    INTEL_GUCRC_FIRMWARE_CONTROL = 1,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_preempt_options {
    INTEL_GUC_PREEMPT_OPTION_DROP_WORK_Q = 0x4,
    INTEL_GUC_PREEMPT_OPTION_DROP_SUBMIT_Q = 0x8,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_report_status {
    INTEL_GUC_REPORT_STATUS_UNKNOWN = 0x0,
    INTEL_GUC_REPORT_STATUS_ACKED = 0x1,
    INTEL_GUC_REPORT_STATUS_ERROR = 0x2,
    INTEL_GUC_REPORT_STATUS_COMPLETE = 0x4,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_sleep_state_status {
    INTEL_GUC_SLEEP_STATE_SUCCESS = 0x1,
    INTEL_GUC_SLEEP_STATE_PREEMPT_TO_IDLE_FAILED = 0x2,
    INTEL_GUC_SLEEP_STATE_ENGINE_RESET_FAILED = 0x3,
}
pub const INTEL_GUC_SLEEP_STATE_INVALID_MASK: u32 = 0x8000_0000;

pub const GUC_LOG_CONTROL_LOGGING_ENABLED: u32 = 1 << 0;
pub const GUC_LOG_CONTROL_VERBOSITY_SHIFT: u32 = 4;
pub const GUC_LOG_CONTROL_VERBOSITY_MASK: u32 = 0xF << GUC_LOG_CONTROL_VERBOSITY_SHIFT;
pub const GUC_LOG_CONTROL_DEFAULT_LOGGING: u32 = 1 << 8;

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_state_capture_event_status {
    INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_SUCCESS = 0x0,
    INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_NOSPACE = 0x1,
}
pub const INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_MASK: u32 = 0x0000_00ff;

pub const INTEL_GUC_TLB_INVAL_TYPE_MASK: u32 = reg_genmask(7, 0);
pub const INTEL_GUC_TLB_INVAL_MODE_MASK: u32 = reg_genmask(11, 8);
pub const INTEL_GUC_TLB_INVAL_FLUSH_CACHE: u32 = reg_bit(31);

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_tlb_invalidation_type {
    INTEL_GUC_TLB_INVAL_ENGINES = 0x0,
    INTEL_GUC_TLB_INVAL_GUC = 0x3,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum intel_guc_tlb_inval_mode {
    INTEL_GUC_TLB_INVAL_MODE_HEAVY = 0x0,
    INTEL_GUC_TLB_INVAL_MODE_LITE = 0x1,
}

// Rust-style aliases retain the C tag spellings above for call sites that
// mirror upstream source names.
pub type IntelGucAction = intel_guc_action;
pub type IntelGucRcOptions = intel_guc_rc_options;
pub type IntelGucPreemptOptions = intel_guc_preempt_options;
pub type IntelGucReportStatus = intel_guc_report_status;
pub type IntelGucSleepStateStatus = intel_guc_sleep_state_status;
pub type IntelGucStateCaptureEventStatus = intel_guc_state_capture_event_status;
pub type IntelGucTlbInvalidationType = intel_guc_tlb_invalidation_type;
pub type IntelGucTlbInvalMode = intel_guc_tlb_inval_mode;

const _: [(); 4] = [(); core::mem::size_of::<intel_guc_action>()];
const _: [(); 4] = [(); core::mem::size_of::<intel_guc_tlb_invalidation_type>()];
