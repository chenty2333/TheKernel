// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Source-order register definitions from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_engine_regs.h.

#![allow(non_snake_case)]

use crate::{
    intel_workarounds_types_upstream::I915RegT,
    linux::registers::{BLT_RING_BASE, GEN6_BSD_RING_BASE, RENDER_RING_BASE, VEBOX_RING_BASE},
    linux_config::PAGE_SIZE,
};

const fn mmio(base: u32, offset: u32) -> I915RegT {
    I915RegT { reg: base + offset }
}

pub const fn RING_EXCC(base: u32) -> I915RegT {
    mmio(base, 0x28)
}
pub const fn RING_TAIL(base: u32) -> I915RegT {
    mmio(base, 0x30)
}
pub const TAIL_ADDR: u32 = 0x001F_FFF8;
pub const fn RING_HEAD(base: u32) -> I915RegT {
    mmio(base, 0x34)
}
pub const HEAD_WRAP_COUNT: u32 = 0xFFE0_0000;
pub const HEAD_WRAP_ONE: u32 = 0x0020_0000;
pub const HEAD_ADDR: u32 = 0x001F_FFFC;
pub const HEAD_WAIT_I8XX: u32 = 1 << 0;
pub const fn RING_START(base: u32) -> I915RegT {
    mmio(base, 0x38)
}
pub const fn RING_CTL(base: u32) -> I915RegT {
    mmio(base, 0x3c)
}
pub const fn RING_CTL_SIZE(size: u32) -> u32 {
    size - PAGE_SIZE as u32
}
pub const RING_NR_PAGES: u32 = 0x001F_F000;
pub const RING_REPORT_MASK: u32 = 0x0000_0006;
pub const RING_REPORT_64K: u32 = 0x0000_0002;
pub const RING_REPORT_128K: u32 = 0x0000_0004;
pub const RING_NO_REPORT: u32 = 0;
pub const RING_VALID_MASK: u32 = 1;
pub const RING_VALID: u32 = 1;
pub const RING_INVALID: u32 = 0;
pub const RING_WAIT: u32 = 1 << 11;
pub const RING_WAIT_SEMAPHORE: u32 = 1 << 10;

pub const fn RING_SYNC_0(base: u32) -> I915RegT {
    mmio(base, 0x40)
}
pub const fn RING_SYNC_1(base: u32) -> I915RegT {
    mmio(base, 0x44)
}
pub const fn RING_SYNC_2(base: u32) -> I915RegT {
    mmio(base, 0x48)
}
pub const GEN6_RVSYNC: I915RegT = RING_SYNC_0(RENDER_RING_BASE);
pub const GEN6_RBSYNC: I915RegT = RING_SYNC_1(RENDER_RING_BASE);
pub const GEN6_RVESYNC: I915RegT = RING_SYNC_2(RENDER_RING_BASE);
pub const GEN6_VBSYNC: I915RegT = RING_SYNC_0(GEN6_BSD_RING_BASE);
pub const GEN6_VRSYNC: I915RegT = RING_SYNC_1(GEN6_BSD_RING_BASE);
pub const GEN6_VVESYNC: I915RegT = RING_SYNC_2(GEN6_BSD_RING_BASE);
pub const GEN6_BRSYNC: I915RegT = RING_SYNC_0(BLT_RING_BASE);
pub const GEN6_BVSYNC: I915RegT = RING_SYNC_1(BLT_RING_BASE);
pub const GEN6_BVESYNC: I915RegT = RING_SYNC_2(BLT_RING_BASE);
pub const GEN6_VEBSYNC: I915RegT = RING_SYNC_0(VEBOX_RING_BASE);
pub const GEN6_VERSYNC: I915RegT = RING_SYNC_1(VEBOX_RING_BASE);
pub const GEN6_VEVSYNC: I915RegT = RING_SYNC_2(VEBOX_RING_BASE);

pub const fn RING_PSMI_CTL(base: u32) -> I915RegT {
    mmio(base, 0x50)
}
pub const GEN8_RC_SEMA_IDLE_MSG_DISABLE: u32 = 1 << 12;
pub const GEN8_FF_DOP_CLOCK_GATE_DISABLE: u32 = 1 << 10;
pub const GEN12_WAIT_FOR_EVENT_POWER_DOWN_DISABLE: u32 = 1 << 7;
pub const GEN6_BSD_GO_INDICATOR: u32 = 1 << 4;
pub const GEN6_BSD_SLEEP_INDICATOR: u32 = 1 << 3;
pub const GEN6_BSD_SLEEP_FLUSH_DISABLE: u32 = 1 << 2;
pub const GEN6_PSMI_SLEEP_MSG_DISABLE: u32 = 1 << 0;
pub const fn RING_MAX_IDLE(base: u32) -> I915RegT {
    mmio(base, 0x54)
}
pub const fn PWRCTX_MAXCNT(base: u32) -> I915RegT {
    mmio(base, 0x54)
}
pub const IDLE_TIME_MASK: u32 = 0x000F_FFFF;
pub const fn RING_ACTHD_UDW(base: u32) -> I915RegT {
    mmio(base, 0x5c)
}
pub const fn RING_DMA_FADD_UDW(base: u32) -> I915RegT {
    mmio(base, 0x60)
}
pub const fn RING_IPEIR(base: u32) -> I915RegT {
    mmio(base, 0x64)
}
pub const fn RING_IPEHR(base: u32) -> I915RegT {
    mmio(base, 0x68)
}
pub const fn RING_INSTDONE(base: u32) -> I915RegT {
    mmio(base, 0x6c)
}
pub const fn RING_INSTPS(base: u32) -> I915RegT {
    mmio(base, 0x70)
}
pub const fn RING_DMA_FADD(base: u32) -> I915RegT {
    mmio(base, 0x78)
}
pub const fn RING_ACTHD(base: u32) -> I915RegT {
    mmio(base, 0x74)
}
pub const fn RING_HWS_PGA(base: u32) -> I915RegT {
    mmio(base, 0x80)
}
pub const fn RING_CMD_BUF_CCTL(base: u32) -> I915RegT {
    mmio(base, 0x84)
}
pub const fn IPEIR(base: u32) -> I915RegT {
    mmio(base, 0x88)
}
pub const fn IPEHR(base: u32) -> I915RegT {
    mmio(base, 0x8c)
}
pub const fn RING_ID(base: u32) -> I915RegT {
    mmio(base, 0x8c)
}
pub const fn RING_NOPID(base: u32) -> I915RegT {
    mmio(base, 0x94)
}
pub const fn RING_HWSTAM(base: u32) -> I915RegT {
    mmio(base, 0x98)
}
pub const fn RING_MI_MODE(base: u32) -> I915RegT {
    mmio(base, 0x9c)
}
pub const ASYNC_FLIP_PERF_DISABLE: u32 = 1 << 14;
pub const MI_FLUSH_ENABLE: u32 = 1 << 12;
pub const TGL_NESTED_BB_EN: u32 = 1 << 12;
pub const MODE_IDLE: u32 = 1 << 9;
pub const STOP_RING: u32 = 1 << 8;
pub const VS_TIMER_DISPATCH: u32 = 1 << 6;
pub const fn RING_IMR(base: u32) -> I915RegT {
    mmio(base, 0xa8)
}
pub const fn RING_EIR(base: u32) -> I915RegT {
    mmio(base, 0xb0)
}
pub const fn RING_EMR(base: u32) -> I915RegT {
    mmio(base, 0xb4)
}
pub const fn RING_ESR(base: u32) -> I915RegT {
    mmio(base, 0xb8)
}
pub const fn GEN12_STATE_ACK_DEBUG(base: u32) -> I915RegT {
    mmio(base, 0xbc)
}
pub const fn RING_INSTPM(base: u32) -> I915RegT {
    mmio(base, 0xc0)
}
pub const fn RING_CMD_CCTL(base: u32) -> I915RegT {
    mmio(base, 0xc4)
}
pub const fn ACTHD(base: u32) -> I915RegT {
    mmio(base, 0xc8)
}
pub const fn GEN8_R_PWR_CLK_STATE(base: u32) -> I915RegT {
    mmio(base, 0xc8)
}
pub const GEN8_RPCS_ENABLE: u32 = 1 << 31;
pub const GEN8_RPCS_S_CNT_ENABLE: u32 = 1 << 18;
pub const GEN8_RPCS_S_CNT_SHIFT: u32 = 15;
pub const GEN8_RPCS_S_CNT_MASK: u32 = 0x7 << GEN8_RPCS_S_CNT_SHIFT;
pub const GEN11_RPCS_S_CNT_SHIFT: u32 = 12;
pub const GEN11_RPCS_S_CNT_MASK: u32 = 0x3f << GEN11_RPCS_S_CNT_SHIFT;
pub const GEN8_RPCS_SS_CNT_ENABLE: u32 = 1 << 11;
pub const GEN8_RPCS_SS_CNT_SHIFT: u32 = 8;
pub const GEN8_RPCS_SS_CNT_MASK: u32 = 0x7 << GEN8_RPCS_SS_CNT_SHIFT;
pub const GEN8_RPCS_EU_MAX_SHIFT: u32 = 4;
pub const GEN8_RPCS_EU_MAX_MASK: u32 = 0xf << GEN8_RPCS_EU_MAX_SHIFT;
pub const GEN8_RPCS_EU_MIN_SHIFT: u32 = 0;
pub const GEN8_RPCS_EU_MIN_MASK: u32 = 0xf << GEN8_RPCS_EU_MIN_SHIFT;

pub const fn RING_RESET_CTL(base: u32) -> I915RegT {
    mmio(base, 0xd0)
}
pub const RESET_CTL_CAT_ERROR: u32 = 1 << 2;
pub const RESET_CTL_READY_TO_RESET: u32 = 1 << 1;
pub const RESET_CTL_REQUEST_RESET: u32 = 1 << 0;
pub const fn DMA_FADD_I8XX(base: u32) -> I915RegT {
    mmio(base, 0xd0)
}
pub const fn RING_BBSTATE(base: u32) -> I915RegT {
    mmio(base, 0x110)
}
pub const RING_BB_PPGTT: u32 = 1 << 5;
pub const fn RING_SBBADDR(base: u32) -> I915RegT {
    mmio(base, 0x114)
}
pub const fn RING_SBBSTATE(base: u32) -> I915RegT {
    mmio(base, 0x118)
}
pub const fn RING_SBBADDR_UDW(base: u32) -> I915RegT {
    mmio(base, 0x11c)
}
pub const fn RING_BBADDR(base: u32) -> I915RegT {
    mmio(base, 0x140)
}
pub const fn RING_BB_OFFSET(base: u32) -> I915RegT {
    mmio(base, 0x158)
}
pub const fn RING_BBADDR_UDW(base: u32) -> I915RegT {
    mmio(base, 0x168)
}
pub const fn CCID(base: u32) -> I915RegT {
    mmio(base, 0x180)
}
pub const CCID_EN: u32 = 1 << 0;
pub const CCID_EXTENDED_STATE_RESTORE: u32 = 1 << 2;
pub const CCID_EXTENDED_STATE_SAVE: u32 = 1 << 3;
pub const fn RING_BB_PER_CTX_PTR(base: u32) -> I915RegT {
    mmio(base, 0x1c0)
}
pub const PER_CTX_BB_FORCE: u32 = 1 << 2;
pub const PER_CTX_BB_VALID: u32 = 1 << 0;
pub const fn RING_INDIRECT_CTX(base: u32) -> I915RegT {
    mmio(base, 0x1c4)
}
pub const fn RING_INDIRECT_CTX_OFFSET(base: u32) -> I915RegT {
    mmio(base, 0x1c8)
}
pub const fn ECOSKPD(base: u32) -> I915RegT {
    mmio(base, 0x1d0)
}
pub const XEHP_BLITTER_SCHEDULING_MODE_MASK: u32 = 0x3 << 11;
pub const XEHP_BLITTER_ROUND_ROBIN_MODE: u32 = (1 << 11);
pub const ECO_CONSTANT_BUFFER_SR_DISABLE: u32 = 1 << 4;
pub const ECO_GATING_CX_ONLY: u32 = 1 << 3;
pub const GEN6_BLITTER_FBC_NOTIFY: u32 = 1 << 3;
pub const ECO_FLIP_DONE: u32 = 1 << 0;
pub const GEN6_BLITTER_LOCK_SHIFT: u32 = 16;

pub const fn BLIT_CCTL(base: u32) -> I915RegT {
    mmio(base, 0x204)
}
pub const BLIT_CCTL_DST_MOCS_MASK: u32 = 0x7f << 8;
pub const BLIT_CCTL_SRC_MOCS_MASK: u32 = 0x7f;
pub const BLIT_CCTL_MASK: u32 = BLIT_CCTL_DST_MOCS_MASK | BLIT_CCTL_SRC_MOCS_MASK;
pub const fn BLIT_CCTL_MOCS(dst: u32, src: u32) -> u32 {
    (((dst << 1) & 0x7f) << 8) | ((src << 1) & 0x7f)
}
pub const fn RING_CSCMDOP(base: u32) -> I915RegT {
    mmio(base, 0x20c)
}

pub const CMD_CCTL_WRITE_OVERRIDE_MASK: u32 = 0x7f << 7;
pub const CMD_CCTL_READ_OVERRIDE_MASK: u32 = 0x7f;
pub const CMD_CCTL_MOCS_MASK: u32 = CMD_CCTL_WRITE_OVERRIDE_MASK | CMD_CCTL_READ_OVERRIDE_MASK;
pub const fn CMD_CCTL_MOCS_OVERRIDE(write: u32, read: u32) -> u32 {
    (((write << 1) & 0x7f) << 7) | ((read << 1) & 0x7f)
}

pub const fn RING_PREDICATE_RESULT(base: u32) -> I915RegT {
    mmio(base, 0x3b8)
}
pub const fn MI_PREDICATE_RESULT_2(base: u32) -> I915RegT {
    mmio(base, 0x3bc)
}
pub const LOWER_SLICE_ENABLED: u32 = 1;
pub const LOWER_SLICE_DISABLED: u32 = 0;
pub const fn MI_PREDICATE_SRC0(base: u32) -> I915RegT {
    mmio(base, 0x400)
}
pub const fn MI_PREDICATE_SRC0_UDW(base: u32) -> I915RegT {
    mmio(base, 0x404)
}
pub const fn MI_PREDICATE_SRC1(base: u32) -> I915RegT {
    mmio(base, 0x408)
}
pub const fn MI_PREDICATE_SRC1_UDW(base: u32) -> I915RegT {
    mmio(base, 0x40c)
}
pub const fn MI_PREDICATE_DATA(base: u32) -> I915RegT {
    mmio(base, 0x410)
}
pub const fn MI_PREDICATE_RESULT(base: u32) -> I915RegT {
    mmio(base, 0x418)
}
pub const fn MI_PREDICATE_RESULT_1(base: u32) -> I915RegT {
    mmio(base, 0x41c)
}
pub const fn RING_PP_DIR_DCLV(base: u32) -> I915RegT {
    mmio(base, 0x220)
}
pub const PP_DIR_DCLV_2G: u32 = u32::MAX;
pub const fn RING_PP_DIR_BASE(base: u32) -> I915RegT {
    mmio(base, 0x228)
}
pub const fn RING_ELSP(base: u32) -> I915RegT {
    mmio(base, 0x230)
}
pub const fn RING_EXECLIST_STATUS_LO(base: u32) -> I915RegT {
    mmio(base, 0x234)
}
pub const fn RING_EXECLIST_STATUS_HI(base: u32) -> I915RegT {
    mmio(base, 0x238)
}
pub const fn RING_CONTEXT_CONTROL(base: u32) -> I915RegT {
    mmio(base, 0x244)
}
pub const CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT: u32 = 1 << 0;
pub const CTX_CTRL_RS_CTX_ENABLE: u32 = 1 << 1;
pub const CTX_CTRL_ENGINE_CTX_SAVE_INHIBIT: u32 = 1 << 2;
pub const CTX_CTRL_INHIBIT_SYN_CTX_SWITCH: u32 = 1 << 3;
pub const GEN12_CTX_CTRL_RUNALONE_MODE: u32 = 1 << 7;
pub const GEN12_CTX_CTRL_OAR_CONTEXT_ENABLE: u32 = 1 << 8;
pub const fn RING_CTX_SR_CTL(base: u32) -> I915RegT {
    mmio(base, 0x244)
}
pub const fn RING_SEMA_WAIT_POLL(base: u32) -> I915RegT {
    mmio(base, 0x24c)
}
pub const fn GEN8_RING_PDP_UDW(base: u32, n: u32) -> I915RegT {
    mmio(base, 0x270 + n * 8 + 4)
}
pub const fn GEN8_RING_PDP_LDW(base: u32, n: u32) -> I915RegT {
    mmio(base, 0x270 + n * 8)
}
pub const fn RING_MODE_GEN7(base: u32) -> I915RegT {
    mmio(base, 0x29c)
}
pub const GFX_RUN_LIST_ENABLE: u32 = 1 << 15;
pub const GFX_INTERRUPT_STEERING: u32 = 1 << 14;
pub const GFX_TLB_INVALIDATE_EXPLICIT: u32 = 1 << 13;
pub const GFX_SURFACE_FAULT_ENABLE: u32 = 1 << 12;
pub const GFX_REPLAY_MODE: u32 = 1 << 11;
pub const GFX_PSMI_GRANULARITY: u32 = 1 << 10;
pub const GEN12_GFX_PREFETCH_DISABLE: u32 = 1 << 10;
pub const GFX_PPGTT_ENABLE: u32 = 1 << 9;
pub const GEN8_GFX_PPGTT_48B: u32 = 1 << 7;
pub const GFX_FORWARD_VBLANK_MASK: u32 = 3 << 5;
pub const GFX_FORWARD_VBLANK_NEVER: u32 = 0;
pub const GFX_FORWARD_VBLANK_ALWAYS: u32 = 1 << 5;
pub const GFX_FORWARD_VBLANK_COND: u32 = 2 << 5;
pub const GEN11_GFX_DISABLE_LEGACY_MODE: u32 = 1 << 3;
pub const fn RING_TIMESTAMP(base: u32) -> I915RegT {
    mmio(base, 0x358)
}
pub const fn RING_TIMESTAMP_UDW(base: u32) -> I915RegT {
    mmio(base, 0x35c)
}
pub const fn RING_CONTEXT_STATUS_PTR(base: u32) -> I915RegT {
    mmio(base, 0x3a0)
}
pub const fn RING_CTX_TIMESTAMP(base: u32) -> I915RegT {
    mmio(base, 0x3a8)
}
pub const fn MI_PREDICATE_RESULT_2_ENGINE(base: u32) -> I915RegT {
    mmio(base, 0x3bc)
}
pub const fn RING_FORCE_TO_NONPRIV(base: u32, i: u32) -> I915RegT {
    mmio(base, 0x4d0 + i * 4)
}
pub const RING_FORCE_TO_NONPRIV_DENY: u32 = 1 << 30;
pub const RING_FORCE_TO_NONPRIV_ADDRESS_MASK: u32 = 0x03FF_FFFC;
pub const RING_FORCE_TO_NONPRIV_ACCESS_RW: u32 = 0 << 28;
pub const RING_FORCE_TO_NONPRIV_ACCESS_RD: u32 = 1 << 28;
pub const RING_FORCE_TO_NONPRIV_ACCESS_WR: u32 = 2 << 28;
pub const RING_FORCE_TO_NONPRIV_ACCESS_INVALID: u32 = 3 << 28;
pub const RING_FORCE_TO_NONPRIV_ACCESS_MASK: u32 = 3 << 28;
pub const RING_FORCE_TO_NONPRIV_RANGE_1: u32 = 0;
pub const RING_FORCE_TO_NONPRIV_RANGE_4: u32 = 1;
pub const RING_FORCE_TO_NONPRIV_RANGE_16: u32 = 2;
pub const RING_FORCE_TO_NONPRIV_RANGE_64: u32 = 3;
pub const RING_FORCE_TO_NONPRIV_RANGE_MASK: u32 = 3;
pub const RING_FORCE_TO_NONPRIV_MASK_VALID: u32 = RING_FORCE_TO_NONPRIV_RANGE_MASK
    | RING_FORCE_TO_NONPRIV_ACCESS_MASK
    | RING_FORCE_TO_NONPRIV_DENY;
pub const RING_MAX_NONPRIV_SLOTS: u32 = 12;
pub const fn RING_EXECLIST_SQ_CONTENTS(base: u32) -> I915RegT {
    mmio(base, 0x510)
}
pub const fn RING_PP_DIR_BASE_READ(base: u32) -> I915RegT {
    mmio(base, 0x518)
}
pub const fn RING_EXECLIST_CONTROL(base: u32) -> I915RegT {
    mmio(base, 0x550)
}
pub const EL_CTRL_LOAD: u32 = 1;
pub const fn GEN8_RING_CS_GPR(base: u32, n: u32) -> I915RegT {
    mmio(base, 0x600 + n * 8)
}
pub const fn GEN8_RING_CS_GPR_UDW(base: u32, n: u32) -> I915RegT {
    mmio(base, 0x600 + n * 8 + 4)
}
pub const fn GEN11_VCS_SFC_FORCED_LOCK(base: u32) -> I915RegT {
    mmio(base, 0x88c)
}
pub const GEN11_VCS_SFC_FORCED_LOCK_BIT: u32 = 1;
pub const fn GEN11_VCS_SFC_LOCK_STATUS(base: u32) -> I915RegT {
    mmio(base, 0x890)
}
pub const GEN11_VCS_SFC_USAGE_BIT: u32 = 1;
pub const GEN11_VCS_SFC_LOCK_ACK_BIT: u32 = 1 << 1;
pub const fn GEN11_VECS_SFC_FORCED_LOCK(base: u32) -> I915RegT {
    mmio(base, 0x201c)
}
pub const GEN11_VECS_SFC_FORCED_LOCK_BIT: u32 = 1;
pub const fn GEN11_VECS_SFC_LOCK_ACK(base: u32) -> I915RegT {
    mmio(base, 0x2018)
}
pub const GEN11_VECS_SFC_LOCK_ACK_BIT: u32 = 1;
pub const fn GEN11_VECS_SFC_USAGE(base: u32) -> I915RegT {
    mmio(base, 0x2014)
}
pub const GEN11_VECS_SFC_USAGE_BIT: u32 = 1;
pub const fn RING_HWS_PGA_GEN6(base: u32) -> I915RegT {
    mmio(base, 0x2080)
}
pub const fn GEN12_HCP_SFC_LOCK_STATUS(base: u32) -> I915RegT {
    mmio(base, 0x2914)
}
pub const GEN12_HCP_SFC_LOCK_ACK_BIT: u32 = 1 << 1;
pub const GEN12_HCP_SFC_USAGE_BIT: u32 = 1;
pub const fn VDBOX_CGCTL3F10(base: u32) -> I915RegT {
    mmio(base, 0x3f10)
}
pub const IECPUNIT_CLKGATE_DIS: u32 = 1 << 22;
pub const fn VDBOX_CGCTL3F18(base: u32) -> I915RegT {
    mmio(base, 0x3f18)
}
pub const ALNUNIT_CLKGATE_DIS: u32 = 1 << 13;
pub const fn VDBOX_CGCTL3F1C(base: u32) -> I915RegT {
    mmio(base, 0x3f1c)
}
pub const MFXPIPE_CLKGATE_DIS: u32 = 1 << 3;

pub const XEHP_CCS_MODE_CSLICE_MASK: u32 = 0x7;
pub const fn XEHP_CCS_MODE_CSLICE(cslice: u32, ccs: u32) -> u32 {
    ccs << (cslice * 3)
}
pub const fn L3_GENERAL_PRIO_CREDITS(value: u32) -> u32 {
    (value >> 1) << 19
}
pub const fn L3_HIGH_PRIO_CREDITS(value: u32) -> u32 {
    (value >> 1) << 14
}

const _: [(); 4] = [(); core::mem::size_of::<I915RegT>()];
