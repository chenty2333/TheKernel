// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux 7.2.3 i915 register/command definitions used by the imported GT
//! sources. Register constructors return the same typed offset wrapper as
//! `i915_reg_defs.h`; command words retain their hardware encoding.

#![allow(non_snake_case)]

use crate::intel_workarounds_upstream::{I915McrReg, I915Reg};

/// Linux i915 `_MMIO(offset)` register constructor.
#[inline]
#[allow(non_snake_case)]
pub const fn _MMIO(reg: u32) -> I915Reg {
    I915Reg { reg }
}

pub const RENDER_RING_BASE: u32 = 0x02000;
pub const MI_NOOP: u32 = 0;
pub const MI_ARB_ON_OFF: u32 = 0x08 << 23;
pub const MI_ARB_ENABLE: u32 = 1;
pub const MI_ARB_DISABLE: u32 = 0;
pub const MI_LRI_LRM_CS_MMIO: u32 = 1 << 19;
pub const MI_SRM_LRM_GLOBAL_GTT: u32 = 1 << 22;
pub const MI_BATCH_BUFFER_END: u32 = 0x0a << 23;
pub const MI_SEMAPHORE_WAIT: u32 = (0x1c << 23) | 2;
pub const MI_SEMAPHORE_POLL: u32 = 1 << 15;
pub const MI_SEMAPHORE_GLOBAL_GTT: u32 = 1 << 22;

pub const STOP_RING: u32 = 1 << 8;
pub const MODE_IDLE: u32 = 1 << 9;
pub const RING_FORCE_TO_NONPRIV_ACCESS_RD: u32 = 1 << 28;
pub const I915_ENGINE_HAS_RCS_REG_STATE: u32 = 1 << 9;
pub const I915_ENGINE_HAS_EU_PRIORITY: u32 = 1 << 10;
pub const I915_ENGINE_FIRST_RENDER_COMPUTE: u32 = 1 << 11;
pub const I915_ENGINE_CLASS_INVALID_VIRTUAL: i32 = -2;
pub const I915_ENGINE_SUPPORTS_STATS: u32 = 1 << 1;
pub const I915_ENGINE_HAS_PREEMPTION: u32 = 1 << 2;
pub const I915_ENGINE_HAS_SEMAPHORES: u32 = 1 << 3;
pub const I915_ENGINE_HAS_TIMESLICES: u32 = 1 << 4;
pub const I915_ENGINE_HAS_RELATIVE_MMIO: u32 = 1 << 6;
pub const I915_ENGINE_WANT_FORCED_PREEMPTION: u32 = 1 << 8;
pub const I915_ENGINE_USES_WA_HOLD_SWITCHOUT: u32 = 1 << 12;
pub const I915_PRIORITY_NORMAL: i32 = 0;
pub const I915_PRIORITY_INVALID: i32 = i32::MIN;
pub const I915_PRIORITY_UNPREEMPTABLE: i32 = i32::MAX;
pub const GUC_CLIENT_PRIORITY_KMD_HIGH: usize = 0;
pub const GUC_CLIENT_PRIORITY_HIGH: usize = 1;
pub const GUC_CLIENT_PRIORITY_KMD_NORMAL: usize = 2;
pub const GUC_CLIENT_PRIORITY_NORMAL: usize = 3;
pub const GUC_CLIENT_PRIORITY_NUM: usize = 4;
pub const GUC_CONTEXT_POLICIES_KLV_NUM_IDS: usize = 5;
pub const GUC_PRIO_FINI: u8 = 0xfe;
pub const GUC_PRIO_INIT: u8 = 0xff;
pub const WQ_LEN_MASK: u32 = 0x07ff_0000;
pub const WQ_TYPE_MASK: u32 = 0xff;
pub const WQ_TYPE_BATCH_BUF: u32 = 1;
pub const WQ_TYPE_NOOP: u32 = 4;
pub const GUC_CONTEXT_DISABLE: u32 = 0;
pub const GUC_CONTEXT_ENABLE: u32 = 1;
pub const G2H_LEN_DW_SCHED_CONTEXT_MODE_SET: u32 = 2;
pub const G2H_LEN_DW_DEREGISTER_CONTEXT: u32 = 1;
pub const G2H_LEN_DW_INVALIDATE_TLB: u32 = 1;
pub const GUC_SCHEDULING_POLICIES_KLV_ID_RENDER_COMPUTE_YIELD: u32 = 0x1001;
pub const GUC_CONTEXT_POLICIES_KLV_ID_EXECUTION_QUANTUM: u32 = 0x2001;
pub const GUC_CONTEXT_POLICIES_KLV_ID_PREEMPTION_TIMEOUT: u32 = 0x2002;
pub const GUC_CONTEXT_POLICIES_KLV_ID_SCHEDULING_PRIORITY: u32 = 0x2003;
pub const GUC_CONTEXT_POLICIES_KLV_ID_PREEMPT_TO_IDLE_ON_QUANTUM_EXPIRY: u32 = 0x2004;
pub const GUC_CONTEXT_POLICIES_KLV_ID_SLPM_GT_FREQUENCY: u32 = 0x2005;
pub const INTEL_GUC_ACTION_SCHED_CONTEXT: u32 = 0x1000;
pub const INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_SET: u32 = 0x1001;
pub const INTEL_GUC_ACTION_SCHED_CONTEXT_MODE_DONE: u32 = 0x1002;
pub const INTEL_GUC_ACTION_UPDATE_SCHEDULING_POLICIES_KLV: u32 = 0x0509;
pub const I915_VMA_RELEASE_MAP: u32 = 1;
pub const CORE_DUMP_FLAG_IS_GUC_CAPTURE: u32 = 1;
pub const CONTEXT_LOW_LATENCY: u32 = 14;
pub const CONTEXT_LRCA_DIRTY: u32 = 9;
pub const CONTEXT_GUC_INIT: u32 = 10;
pub const CONTEXT_POLICY_FLAG_PREEMPT_TO_IDLE_V69: u32 = 1;
pub const I915_ERROR_CAPTURE: u32 = 1;
pub const I915_ERROR_INSTRUCTION: u32 = 1;
pub const I915_RESET_BACKOFF: u32 = 0;
pub const I915_RESET_ENGINE: u32 = 1;
pub const I915_DISPATCH_SECURE: u32 = 1;
pub const I915_BO_ALLOC_PM_VOLATILE: u32 = 1 << 4;
pub const I915_MAP_OVERRIDE: u32 = 1 << 31;
pub const I915_GEM_HWS_PREEMPT: u32 = 0x32;
pub const I915_HWS_CSB_BUF0_INDEX: u32 = 0x10;
pub const ENGINE_PHYSICAL: u32 = 0;
pub const ENGINE_VIRTUAL: u32 = 2;
pub const GEN_DSS_PER_GSLICE: u32 = 4;
pub const OTHER_GSC_INSTANCE: u32 = 6;
pub const EXECLIST_MAX_PORTS: usize = 2;
pub const I915_MAX_VCS: usize = 8;
pub const I915_MAX_VECS: usize = 4;
pub const I915_MAX_CCS: usize = 4;
pub const I915_MAX_RCS: usize = 1;
pub const I915_MAX_BCS: usize = 9;

#[allow(non_snake_case)]
pub const fn MI_LOAD_REGISTER_IMM(registers: u32) -> u32 {
    (0x22 << 23) | (2 * registers - 1)
}

#[allow(non_snake_case)]
pub const fn RING_MI_MODE(base: u32) -> I915Reg {
    I915Reg { reg: base + 0x9c }
}

#[allow(non_snake_case)]
pub const fn CCID(base: u32) -> I915Reg {
    I915Reg { reg: base + 0x180 }
}

#[allow(non_snake_case)]
pub const fn REG_FIELD_PREP(mask: u32, value: u32) -> u32 {
    if mask == 0 {
        0
    } else {
        (value << mask.trailing_zeros()) & mask
    }
}

#[allow(non_snake_case)]
pub const fn REG_FIELD_GET(mask: u32, value: u32) -> u32 {
    if mask == 0 {
        0
    } else {
        (value & mask) >> mask.trailing_zeros()
    }
}

// Linux drivers/gpu/drm/i915/gt/intel_lrc.h, intel_lrc_reg.h, and intel_gtt.h
// (Linux 7.2.3). The translated consumers index u32 arrays and perform byte
// pointer arithmetic, so C's integer indices/sizes are usize here; encoded
// command words remain u32.
pub const I915_GTT_PAGE_SIZE_4K: usize = 1 << 12;
pub const I915_GTT_PAGE_SIZE_64K: usize = 1 << 16;
pub const I915_GTT_PAGE_SIZE_2M: usize = 1 << 21;
pub const I915_GTT_PAGE_SIZE: usize = I915_GTT_PAGE_SIZE_4K;
pub const I915_GTT_MAX_PAGE_SIZE: usize = I915_GTT_PAGE_SIZE_2M;
pub const LRC_PPHWSP_PN: usize = 0;
pub const LRC_PPHWSP_SZ: usize = 1;
pub const LRC_STATE_PN: usize = LRC_PPHWSP_PN + LRC_PPHWSP_SZ;
pub const LRC_STATE_OFFSET: usize = LRC_STATE_PN * crate::linux_config::PAGE_SIZE;
pub const LRC_PPHWSP_SCRATCH: usize = 0x34;
pub const LRC_PPHWSP_SCRATCH_ADDR: usize = LRC_PPHWSP_SCRATCH * core::mem::size_of::<u32>();
pub const CTX_CONTEXT_CONTROL: usize = 0x02 + 1;
pub const CTX_RING_HEAD: usize = 0x04 + 1;
pub const CTX_RING_TAIL: usize = 0x06 + 1;
pub const CTX_RING_START: usize = 0x08 + 1;
pub const CTX_RING_CTL: usize = 0x0a + 1;
pub const CTX_BB_STATE: usize = 0x10 + 1;
pub const CTX_TIMESTAMP: usize = 0x22 + 1;
pub const CTX_PDP3_UDW: usize = 0x24 + 1;
pub const CTX_PDP3_LDW: usize = 0x26 + 1;
pub const CTX_PDP2_UDW: usize = 0x28 + 1;
pub const CTX_PDP2_LDW: usize = 0x2a + 1;
pub const CTX_PDP1_UDW: usize = 0x2c + 1;
pub const CTX_PDP1_LDW: usize = 0x2e + 1;
pub const CTX_PDP0_UDW: usize = 0x30 + 1;
pub const CTX_PDP0_LDW: usize = 0x32 + 1;
pub const CTX_R_PWR_CLK_STATE: usize = 0x42 + 1;
pub const CTX_DESC_FORCE_RESTORE: u64 = 1 << 2;
pub const GEN9_CTX_RING_MI_MODE: u32 = 0x54;
pub const GEN8_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x17;
pub const GEN9_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x26;
pub const GEN10_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x19;
pub const GEN11_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x1a;
pub const GEN12_CTX_RCS_INDIRECT_CTX_OFFSET_DEFAULT: u32 = 0x0d;
pub const GEN8_EXECLISTS_STATUS_BUF: u32 = 0x370;
pub const GEN11_EXECLISTS_STATUS_BUF2: u32 = 0x3c0;
pub const GEN8_CSB_ENTRIES: u32 = 6;
pub const GEN8_CSB_PTR_MASK: u32 = 0x7;
pub const GEN8_CSB_READ_PTR_MASK: u32 = GEN8_CSB_PTR_MASK << 8;
pub const GEN8_CSB_WRITE_PTR_MASK: u32 = GEN8_CSB_PTR_MASK;
pub const GEN11_CSB_ENTRIES: u32 = 12;
pub const GEN11_CSB_PTR_MASK: u32 = 0xf;
pub const GEN11_CSB_READ_PTR_MASK: u32 = GEN11_CSB_PTR_MASK << 8;
pub const GEN11_CSB_WRITE_PTR_MASK: u32 = GEN11_CSB_PTR_MASK;
pub const MAX_CONTEXT_HW_ID: u32 = 1 << 21;
pub const GEN11_MAX_CONTEXT_HW_ID: u32 = 1 << 11;
pub const GEN12_MAX_CONTEXT_HW_ID: u32 = GEN11_MAX_CONTEXT_HW_ID - 1;
pub const XEHP_MAX_CONTEXT_HW_ID: u32 = 0xffff;
pub const GEN8_CTX_VALID: u32 = 1 << 0;
pub const GEN8_CTX_FORCE_PD_RESTORE: u32 = 1 << 1;
pub const GEN8_CTX_FORCE_RESTORE: u32 = 1 << 2;
pub const GEN8_CTX_L3LLC_COHERENT: u32 = 1 << 5;
pub const GEN8_CTX_PRIVILEGE: u32 = 1 << 8;
pub const GEN8_CTX_ADDRESSING_MODE_SHIFT: u32 = 3;
pub const CTX_GTT_ADDRESS_MASK: u32 = genmask(31, 12);
pub const GEN12_CTX_PRIORITY_MASK: u32 = genmask(10, 9);
pub const GEN12_CTX_PRIORITY_HIGH: u32 = REG_FIELD_PREP(GEN12_CTX_PRIORITY_MASK, 2);
pub const GEN12_CTX_PRIORITY_NORMAL: u32 = REG_FIELD_PREP(GEN12_CTX_PRIORITY_MASK, 1);
pub const GEN12_CTX_PRIORITY_LOW: u32 = REG_FIELD_PREP(GEN12_CTX_PRIORITY_MASK, 0);
pub const GEN8_CTX_ID_SHIFT: u32 = 32;
pub const GEN8_CTX_ID_WIDTH: u32 = 21;
pub const GEN11_SW_CTX_ID_SHIFT: u32 = 37;
pub const GEN11_SW_CTX_ID_WIDTH: u32 = 11;
pub const GEN11_ENGINE_CLASS_SHIFT: u32 = 61;
pub const GEN11_ENGINE_CLASS_WIDTH: u32 = 3;
pub const GEN11_ENGINE_INSTANCE_SHIFT: u32 = 48;
pub const GEN11_ENGINE_INSTANCE_WIDTH: u32 = 6;
pub const XEHP_SW_CTX_ID_SHIFT: u32 = 39;
pub const XEHP_SW_CTX_ID_WIDTH: u32 = 16;
pub const XEHP_SW_COUNTER_SHIFT: u32 = 58;
pub const XEHP_SW_COUNTER_WIDTH: u32 = 6;
pub const GEN12_GUC_SW_CTX_ID_SHIFT: u32 = 39;
pub const GEN12_GUC_SW_CTX_ID_WIDTH: u32 = 16;
pub const GEN8_3LVL_PDPES: u32 = 4;

// Linux drivers/gpu/drm/i915/gt/intel_engine_regs.h. Every function below
// corresponds to `_MMIO(offset)` and therefore returns a typed I915Reg, not
// the raw integer offset. Bit masks and ring-state words are plain u32.
const fn mmio(offset: u32) -> I915Reg {
    I915Reg { reg: offset }
}

const fn mcr(offset: u32) -> I915McrReg {
    I915McrReg { reg: offset }
}

#[allow(non_snake_case)]
pub const fn RING_TAIL(base: u32) -> I915Reg {
    mmio(base + 0x30)
}
#[allow(non_snake_case)]
pub const fn RING_HEAD(base: u32) -> I915Reg {
    mmio(base + 0x34)
}
#[allow(non_snake_case)]
pub const fn RING_START(base: u32) -> I915Reg {
    mmio(base + 0x38)
}
#[allow(non_snake_case)]
pub const fn RING_CTL(base: u32) -> I915Reg {
    mmio(base + 0x3c)
}
#[allow(non_snake_case)]
pub const fn RING_PSMI_CTL(base: u32) -> I915Reg {
    mmio(base + 0x50)
}
#[allow(non_snake_case)]
pub const fn RING_ACTHD_UDW(base: u32) -> I915Reg {
    mmio(base + 0x5c)
}
#[allow(non_snake_case)]
pub const fn RING_DMA_FADD_UDW(base: u32) -> I915Reg {
    mmio(base + 0x60)
}
#[allow(non_snake_case)]
pub const fn RING_IPEIR(base: u32) -> I915Reg {
    mmio(base + 0x64)
}
#[allow(non_snake_case)]
pub const fn RING_IPEHR(base: u32) -> I915Reg {
    mmio(base + 0x68)
}
#[allow(non_snake_case)]
pub const fn RING_INSTDONE(base: u32) -> I915Reg {
    mmio(base + 0x6c)
}
#[allow(non_snake_case)]
pub const fn RING_DMA_FADD(base: u32) -> I915Reg {
    mmio(base + 0x78)
}
#[allow(non_snake_case)]
pub const fn RING_ACTHD(base: u32) -> I915Reg {
    mmio(base + 0x74)
}
#[allow(non_snake_case)]
pub const fn RING_HWS_PGA(base: u32) -> I915Reg {
    mmio(base + 0x80)
}
#[allow(non_snake_case)]
pub const fn RING_CMD_BUF_CCTL(base: u32) -> I915Reg {
    mmio(base + 0x84)
}
#[allow(non_snake_case)]
pub const fn RING_NOPID(base: u32) -> I915Reg {
    mmio(base + 0x94)
}
#[allow(non_snake_case)]
pub const fn RING_HWSTAM(base: u32) -> I915Reg {
    mmio(base + 0x98)
}
#[allow(non_snake_case)]
pub const fn RING_IMR(base: u32) -> I915Reg {
    mmio(base + 0xa8)
}
#[allow(non_snake_case)]
pub const fn RING_EIR(base: u32) -> I915Reg {
    mmio(base + 0xb0)
}
#[allow(non_snake_case)]
pub const fn RING_EMR(base: u32) -> I915Reg {
    mmio(base + 0xb4)
}
#[allow(non_snake_case)]
pub const fn RING_ESR(base: u32) -> I915Reg {
    mmio(base + 0xb8)
}
#[allow(non_snake_case)]
pub const fn RING_CMD_CCTL(base: u32) -> I915Reg {
    mmio(base + 0xc4)
}
#[allow(non_snake_case)]
pub const fn RING_BBADDR(base: u32) -> I915Reg {
    mmio(base + 0x140)
}
#[allow(non_snake_case)]
pub const fn RING_BBADDR_UDW(base: u32) -> I915Reg {
    mmio(base + 0x168)
}
#[allow(non_snake_case)]
pub const fn RING_ELSP(base: u32) -> I915Reg {
    mmio(base + 0x230)
}
#[allow(non_snake_case)]
pub const fn RING_EXECLIST_STATUS_HI(base: u32) -> I915Reg {
    mmio(base + 0x238)
}
#[allow(non_snake_case)]
pub const fn RING_EXECLIST_CONTROL(base: u32) -> I915Reg {
    mmio(base + 0x550)
}
#[allow(non_snake_case)]
pub const fn RING_CONTEXT_STATUS_PTR(base: u32) -> I915Reg {
    mmio(base + 0x3a0)
}
#[allow(non_snake_case)]
pub const fn RING_CTX_TIMESTAMP(base: u32) -> I915Reg {
    mmio(base + 0x3a8)
}
#[allow(non_snake_case)]
pub const fn RING_SEMA_WAIT_POLL(base: u32) -> I915Reg {
    mmio(base + 0x24c)
}
#[allow(non_snake_case)]
pub const fn RING_EXECLIST_SQ_CONTENTS(base: u32) -> I915Reg {
    mmio(base + 0x510)
}
#[allow(non_snake_case)]
pub const fn RING_MODE_GEN7(base: u32) -> I915Reg {
    mmio(base + 0x29c)
}
#[allow(non_snake_case)]
pub const fn RING_FORCE_TO_NONPRIV(base: u32, index: u32) -> I915Reg {
    mmio(base + 0x4d0 + index * 4)
}
#[allow(non_snake_case)]
pub const fn GEN8_RING_CS_GPR(base: u32, index: u32) -> I915Reg {
    mmio(base + 0x600 + index * 8)
}
#[allow(non_snake_case)]
pub const fn GEN8_RING_PDP_UDW(base: u32, index: u32) -> I915Reg {
    mmio(base + 0x270 + index * 8 + 4)
}
#[allow(non_snake_case)]
pub const fn GEN8_RING_PDP_LDW(base: u32, index: u32) -> I915Reg {
    mmio(base + 0x270 + index * 8)
}
pub const TAIL_ADDR: u32 = 0x001f_fff8;
pub const HEAD_WRAP_COUNT: u32 = 0xffe0_0000;
pub const HEAD_WRAP_ONE: u32 = 0x0020_0000;
pub const HEAD_ADDR: u32 = 0x001f_fffc;
pub const RING_NR_PAGES: u32 = 0x001f_f000;
pub const RING_REPORT_MASK: u32 = 0x0000_0006;
pub const RING_REPORT_64K: u32 = 0x0000_0002;
pub const RING_REPORT_128K: u32 = 0x0000_0004;
pub const RING_NO_REPORT: u32 = 0;
pub const RING_VALID_MASK: u32 = 1;
pub const RING_VALID: u32 = 1;
pub const RING_INVALID: u32 = 0;
pub const RING_WAIT: u32 = 1 << 11;
pub const RING_WAIT_SEMAPHORE: u32 = 1 << 10;
pub const RING_MAX_NONPRIV_SLOTS: u32 = 12;
pub const RING_FORCE_TO_NONPRIV_ACCESS_RW: u32 = 0 << 28;
pub const RING_FORCE_TO_NONPRIV_RANGE_4: u32 = 1 << 0;
pub const RING_FORCE_TO_NONPRIV_RANGE_MASK: u32 = 3;
pub const RING_FORCE_TO_NONPRIV_ACCESS_MASK: u32 = 3 << 28;
pub const RING_FORCE_TO_NONPRIV_ACCESS_INVALID: u32 = 3 << 28;
pub const RING_FORCE_TO_NONPRIV_DENY: u32 = 1 << 30;
pub const RING_FORCE_TO_NONPRIV_MASK_VALID: u32 = RING_FORCE_TO_NONPRIV_RANGE_MASK
    | RING_FORCE_TO_NONPRIV_ACCESS_MASK
    | RING_FORCE_TO_NONPRIV_DENY;
pub const RING_FORCE_TO_NONPRIV_ADDRESS_MASK: u32 = genmask(25, 2);
pub const RING_FORCE_TO_NONPRIV_ACCESS_WR: u32 = 2 << 28;
pub const RING_FORCE_TO_NONPRIV_RANGE_1: u32 = 0;
pub const RING_FORCE_TO_NONPRIV_RANGE_16: u32 = 2;
pub const RING_FORCE_TO_NONPRIV_RANGE_64: u32 = 3;

#[allow(non_snake_case)]
pub const fn RING_CTL_SIZE(size: u32) -> u32 {
    size - crate::linux_config::PAGE_SIZE as u32
}

// Linux drivers/gpu/drm/i915/gt/intel_gpu_commands.h. MI instruction encodings
// remain u32 command words; MI register addresses are independently typed by
// the register helpers above.
pub const MI_USER_INTERRUPT: u32 = 0x02 << 23;
pub const MI_SET_PREDICATE: u32 = 0x01 << 23;
pub const MI_SET_PREDICATE_DISABLE: u32 = 0;
pub const MI_STORE_DWORD_IMM_GEN4: u32 = (0x20 << 23) | 2;
pub const MI_LOAD_REGISTER_MEM_GEN8: u32 = (0x29 << 23) | 2;
pub const MI_LOAD_REGISTER_REG: u32 = (0x2a << 23) | 1;
pub const MI_STORE_REGISTER_MEM: u32 = (0x24 << 23) | 1;
pub const MI_STORE_REGISTER_MEM_GEN8: u32 = (0x24 << 23) | 2;
pub const MI_BATCH_BUFFER_START_GEN8: u32 = (0x31 << 23) | 1;
pub const MI_LRI_FORCE_POSTED: u32 = 1 << 12;
pub const MI_LRR_SOURCE_CS_MMIO: u32 = 1 << 18;
pub const MI_USE_GGTT: u32 = 1 << 22;
pub const MI_SEMAPHORE_SAD_EQ_SDD: u32 = 4 << 12;
pub const MI_SEMAPHORE_REGISTER_POLL: u32 = 1 << 16;
pub const MI_SEMAPHORE_SAD_GT_SDD: u32 = 0 << 12;
pub const MI_SEMAPHORE_SAD_GTE_SDD: u32 = 1 << 12;
pub const MI_SEMAPHORE_SAD_LT_SDD: u32 = 2 << 12;
pub const MI_SEMAPHORE_SAD_LTE_SDD: u32 = 3 << 12;
pub const MI_SEMAPHORE_SAD_NEQ_SDD: u32 = 5 << 12;
pub const MI_SEMAPHORE_SYNC_MASK: u32 = 3 << 16;
pub const MI_SEMAPHORE_SYNC_INVALID: u32 = 3 << 16;
pub const MI_FLUSH_DW: u32 = (0x26 << 23) | 1;
pub const MI_FLUSH_DW_SIZE: u32 = 3;
pub const MI_FLUSH_DW_PROTECTED_MEM_EN: u32 = 1 << 22;
pub const MI_FLUSH_DW_STORE_INDEX: u32 = 1 << 21;
pub const MI_INVALIDATE_TLB: u32 = 1 << 18;
pub const MI_FLUSH_DW_CCS: u32 = 1 << 16;
pub const MI_FLUSH_DW_OP_STOREDW: u32 = 1 << 14;
pub const MI_FLUSH_DW_OP_MASK: u32 = 3 << 14;
pub const MI_FLUSH_DW_LLC: u32 = 1 << 9;
pub const MI_FLUSH_DW_NOTIFY: u32 = 1 << 8;
pub const MI_INVALIDATE_BSD: u32 = 1 << 7;
pub const MI_FLUSH_DW_USE_GTT: u32 = 1 << 2;
pub const MI_FLUSH_DW_USE_PPGTT: u32 = 0;
pub const MI_BATCH_BUFFER_START: u32 = 0x31 << 23;
pub const MI_BATCH_GTT: u32 = 2 << 6;
pub const MI_BATCH_RESOURCE_STREAMER: u32 = 1 << 10;
pub const MI_BATCH_PREDICATE: u32 = 1 << 15;
pub const MI_OPCODE_MASK: u32 = 0x3f << 23;
pub const MI_LOAD_REGISTER_IMM_MAX_REGS: u32 = 126;

// Linux drivers/gpu/drm/i915/gt/intel_gt_regs.h. `_MMIO` definitions are
// I915Reg; `MCR_REG` definitions are I915McrReg, preserving the C type split.
pub const COMMON_SLICE_CHICKEN4: I915Reg = mmio(0x7300);
pub const _3D_CHICKEN: I915Reg = mmio(0x2084);
pub const _3D_CHICKEN_HIZ_PLANE_DISABLE_MSAA_4X_SNB: u32 = 1 << 10;
pub const _3D_CHICKEN2: I915Reg = mmio(0x208c);
pub const _3D_CHICKEN2_WM_READ_PIPELINED: u32 = 1 << 14;
pub const _3D_CHICKEN3: I915Reg = mmio(0x2090);
pub const _3D_CHICKEN_SF_PROVOKING_VERTEX_FIX: u32 = 1 << 12;
pub const _3D_CHICKEN_SF_DISABLE_OBJEND_CULL: u32 = 1 << 10;
pub const _3D_CHICKEN3_AA_LINE_QUALITY_FIX_ENABLE: u32 = 1 << 5;
pub const _3D_CHICKEN3_SF_DISABLE_FASTCLIP_CULL: u32 = 1 << 5;
pub const _3D_CHICKEN3_SF_DISABLE_PIPELINED_ATTR_FETCH: u32 = 1 << 1;
pub const COMMON_SLICE_CHICKEN2: I915Reg = mmio(0x7014);
pub const GEN8_CS_CHICKEN1: I915Reg = mmio(0x2580);
pub const GEN8_GARBCNTL: I915Reg = mmio(0xb004);
pub const GEN8_GAMW_ECO_DEV_RW_IA: I915Reg = mmio(0x4080);
pub const GEN8_L3CNTLREG: I915Reg = mmio(0x7034);
pub const GEN8_HDC_CHICKEN1: I915Reg = mmio(0x7304);
pub const GEN8_MCR_SELECTOR: I915Reg = mmio(0x0fdc);
pub const GEN8_RTCR: I915Reg = mmio(0x4260);
pub const GEN8_M1TCR: I915Reg = mmio(0x4264);
pub const GEN8_M2TCR: I915Reg = mmio(0x4268);
pub const GEN8_BTCR: I915Reg = mmio(0x426c);
pub const GEN8_VTCR: I915Reg = mmio(0x4270);
pub const GEN9_CSFE_CHICKEN1_RCS: I915Reg = mmio(0x20d4);
pub const GEN9_CS_DEBUG_MODE1: I915Reg = mmio(0x20ec);
pub const GEN9_SLICE_COMMON_ECO_CHICKEN1: I915Reg = mmio(0x731c);
pub const GEN9_WM_CHICKEN3: I915Reg = mmio(0x5588);
pub const GEN9_PWRGT_DOMAIN_STATUS: I915Reg = mmio(0xa2a0);
pub const GEN9_CTX_PREEMPT_REG: I915Reg = mmio(0x2248);
pub const GEN9_SCRATCH_LNCF1: I915Reg = mmio(0xb008);
pub const GEN9_GAMT_ECO_REG_RW_IA: I915Reg = mmio(0x4ab0);
pub const GEN7_FF_THREAD_MODE: I915Reg = mmio(0x20a0);
pub const GEN7_UCGCTL4: I915Reg = mmio(0x940c);
pub const GEN7_MISCCPCTL: I915Reg = mmio(0x9424);
pub const GEN7_GT_MODE: I915Reg = mmio(0x7008);
pub const GEN7_COMMON_SLICE_CHICKEN1: I915Reg = mmio(0x7010);
pub const GEN7_SC_INSTDONE: I915Reg = mmio(0x7100);
pub const GEN7_SARCHKMD: I915Reg = mmio(0xb000);
pub const GEN7_SAMPLER_INSTDONE: I915Reg = mmio(0xe160);
pub const GEN7_ROW_INSTDONE: I915Reg = mmio(0xe164);
pub const GEN7_L3SQCREG1: I915Reg = mmio(0xb010);
pub const GEN7_L3CNTLREG1: I915Reg = mmio(0xb01c);
pub const GEN7_L3_CHICKEN_MODE_REGISTER: I915Reg = mmio(0xb030);
pub const GEN7_FF_SLICE_CS_CHICKEN1: I915Reg = mmio(0x20e0);
pub const GEN7_CXT_SIZE: I915Reg = mmio(0x21a8);
pub const GEN7_L3SQCREG4: I915Reg = mmio(0xb034);
pub const GEN7_HALF_SLICE_CHICKEN1: I915Reg = mmio(0xe100);
pub const GEN6_GT_MODE: I915Reg = mmio(0x20d0);
pub const GEN4_INSTDONE1: I915Reg = mmio(0x207c);
pub const GEN2_INSTDONE: I915Reg = mmio(0x2090);
pub const GEN11_COMMON_SLICE_CHICKEN3: I915Reg = mmio(0x7304);
pub const GEN11_SLICE_UNIT_LEVEL_CLKGATE: I915Reg = mmio(0x94d4);
pub const GEN11_LSN_UNSLCVC: I915Reg = mmio(0xb43c);
pub const GEN11_GT_VEBOX_VDBOX_DISABLE: I915Reg = mmio(0x9140);
pub const GEN11_GLBLINVL: I915Reg = mmio(0xb404);
pub const GEN11_GACB_PERF_CTRL: I915Reg = mmio(0x4b80);
pub const GEN12_SQCNT1: I915Reg = mmio(0x8718);
pub const GEN12_RCU_MODE: I915Reg = mmio(0x14800);
pub const GEN12_GFX_TLB_INV_CR: I915Reg = mmio(0xced8);
pub const GEN12_VD_TLB_INV_CR: I915Reg = mmio(0xcedc);
pub const GEN12_VE_TLB_INV_CR: I915Reg = mmio(0xcee0);
pub const GEN12_BLT_TLB_INV_CR: I915Reg = mmio(0xcee4);
pub const GEN12_COMPCTX_TLB_INV_CR: I915Reg = mmio(0xcf04);
pub const GEN12_SC_INSTDONE_EXTRA: I915Reg = mmio(0x7104);
pub const GEN12_SC_INSTDONE_EXTRA2: I915Reg = mmio(0x7108);
pub const GEN12_FF_MODE2: I915Reg = mmio(0x6604);
pub const GEN12_CS_DEBUG_MODE2: I915Reg = mmio(0x20d8);
pub const GEN12_GUC_SEM_INTR_ENABLES: I915Reg = mmio(0xc71c);
pub const GEN8_L3SQCREG1: I915McrReg = mcr(0xb100);
pub const GEN8_L3SQCREG4: I915McrReg = mcr(0xb118);
pub const GEN8_ROW_CHICKEN: I915McrReg = mcr(0xe4f0);
pub const GEN8_ROW_CHICKEN2: I915McrReg = mcr(0xe4f4);
pub const GEN8_HALF_SLICE_CHICKEN1: I915McrReg = mcr(0xe100);
pub const GEN8_HALF_SLICE_CHICKEN3: I915McrReg = mcr(0xe184);
pub const GEN8_WM_CHICKEN2: I915McrReg = mcr(0x5584);
pub const GEN9_ROW_CHICKEN3: I915McrReg = mcr(0xe49c);
pub const GEN9_ROW_CHICKEN4: I915McrReg = mcr(0xe48c);
pub const GEN9_HALF_SLICE_CHICKEN5: I915McrReg = mcr(0xe188);
pub const GEN9_HALF_SLICE_CHICKEN7: I915McrReg = mcr(0xe194);
pub const GEN9_SCRATCH1: I915McrReg = mcr(0xb11c);
pub const GEN10_SAMPLER_MODE: I915McrReg = mcr(0xe18c);
pub const GEN10_CACHE_MODE_SS: I915McrReg = mcr(0xe420);
pub const GEN10_DFR_RATIO_EN_AND_CHICKEN: I915McrReg = mcr(0x9550);
pub const GEN11_SUBSLICE_UNIT_LEVEL_CLKGATE: I915McrReg = mcr(0x9524);
pub const GEN11_SCRATCH2: I915McrReg = mcr(0xb140);

// Ring base offsets and IRQ shifts from Linux drivers/gpu/drm/i915/i915_reg.h.
pub const BSD_RING_BASE: u32 = 0x04000;
pub const GEN6_BSD_RING_BASE: u32 = 0x12000;
pub const VEBOX_RING_BASE: u32 = 0x1a000;
pub const BLT_RING_BASE: u32 = 0x22000;
pub const GEN8_BSD2_RING_BASE: u32 = 0x1c000;
pub const GEN11_BSD_RING_BASE: u32 = 0x1c0000;
pub const GEN11_BSD2_RING_BASE: u32 = 0x1c4000;
pub const GEN11_BSD3_RING_BASE: u32 = 0x1d0000;
pub const GEN11_BSD4_RING_BASE: u32 = 0x1d4000;
pub const GEN11_VEBOX_RING_BASE: u32 = 0x1c8000;
pub const GEN11_VEBOX2_RING_BASE: u32 = 0x1d8000;
pub const GEN12_COMPUTE0_RING_BASE: u32 = 0x1a000;
pub const GEN12_COMPUTE1_RING_BASE: u32 = 0x1c000;
pub const GEN12_COMPUTE2_RING_BASE: u32 = 0x1e000;
pub const GEN12_COMPUTE3_RING_BASE: u32 = 0x26000;
pub const GEN8_RCS_IRQ_SHIFT: u32 = 0;
pub const GEN8_BCS_IRQ_SHIFT: u32 = 16;
pub const GEN8_VCS0_IRQ_SHIFT: u32 = 0;
pub const GEN8_VCS1_IRQ_SHIFT: u32 = 16;
pub const GEN8_VECS_IRQ_SHIFT: u32 = 0;
pub const GEN11_ENABLE_32_PLANE_MODE: u32 = 1 << 7;
pub const GEN11_DIS_PICK_2ND_EU: u32 = 1 << 7;
pub const GEN11_BANK_HASH_ADDR_EXCL_MASK: u32 = 0x7f << 5;
pub const GEN11_BANK_HASH_ADDR_EXCL_BIT0: u32 = 1 << 5;
pub const GEN11_ARBITRATION_PRIO_ORDER_MASK: u32 = genmask(27, 22);
pub const GEN11_GFX_DISABLE_LEGACY_MODE: u32 = 1 << 3;
pub const CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT: u32 = 1;
pub const CTX_CTRL_RS_CTX_ENABLE: u32 = 1 << 1;
pub const CTX_CTRL_ENGINE_CTX_SAVE_INHIBIT: u32 = 1 << 2;
pub const CTX_CTRL_INHIBIT_SYN_CTX_SWITCH: u32 = 1 << 3;
pub const CTX_WA_BB_SIZE: usize = crate::linux_config::PAGE_SIZE;

pub const FORCE_MISS_FTLB: u32 = 1 << 3;
pub const GEN8_SBE_DISABLE_REPLAY_BUF_OPTIMIZATION: u32 = 1 << 8;
pub const GEN8_EU_GAUNIT_CLOCK_GATE_DISABLE: u32 = 1 << 14;
pub const GEN8_SAMPLER_POWER_BYPASS_DIS: u32 = 1 << 1;
pub const GEN8_LQSC_FLUSH_COHERENT_LINES: u32 = 1 << 21;
pub const GEN8_LQSQ_NONIA_COHERENT_ATOMICS_ENABLE: u32 = 1 << 22;
pub const GEN8_4x4_STC_OPTIMIZATION_DISABLE: u32 = 1 << 6;
pub const GEN8_ST_PO_DISABLE: u32 = 1 << 13;
pub const GEN8_RC_SEMA_IDLE_MSG_DISABLE: u32 = 1 << 12;
pub const GEN8_GRDOM_MEDIA2: u32 = 1 << 7;
pub const GEN8_ERRDETBCTRL: u32 = 1 << 9;
pub const GEN9_PREEMPT_GPGPU_LEVEL_MASK: u32 = (1 << 2) | (1 << 1);
pub const GEN9_PREEMPT_GPGPU_THREAD_GROUP_LEVEL: u32 = (0 << 2) | (1 << 1);
pub const GEN9_PREEMPT_GPGPU_COMMAND_LEVEL: u32 = (1 << 2) | (0 << 1);
pub const GEN9_PREEMPT_GPGPU_SYNC_SWITCH_DISABLE: u32 = 1 << 2;
pub const GEN9_PREEMPT_3D_OBJECT_LEVEL: u32 = 1;
pub const fn GEN9_IZ_HASHING_MASK(slice: u32) -> u32 {
    0x3 << (slice * 2)
}
pub const fn GEN9_IZ_HASHING(slice: u32, value: u32) -> u32 {
    value << (slice * 2)
}
pub const GEN9_POOLED_EU_LOAD_BALANCING_FIX_DISABLE: u32 = 1 << 10;
pub const GEN9_PBE_COMPRESSED_HASH_SELECTION: u32 = 1 << 13;
pub const GEN9_PARTIAL_RESOLVE_IN_VC_DISABLE: u32 = 1 << 1;
pub const GEN9_MEDIA_POOL_STATE: u32 = (0x3 << 29) | (0x2 << 27) | (0x5 << 16) | 4;
pub const GEN9_MEDIA_POOL_ENABLE: u32 = 1 << 31;
pub const GEN9_LNCF_NONIA_COHERENT_ATOMICS_ENABLE: u32 = 1;
pub const GEN9_LBS_SLA_RETRY_TIMER_DECREMENT_ENABLE: u32 = 1 << 2;
pub const GEN9_GAPS_TSV_CREDIT_DISABLE: u32 = 1 << 7;
pub const GEN9_FFSC_PERCTX_PREEMPT_CTRL: u32 = 1 << 14;
pub const GEN9_FACTOR_IN_CLR_VAL_HIZ: u32 = 1 << 9;
pub const GEN9_ENABLE_YV12_BUGFIX: u32 = 1 << 4;
pub const GEN9_ENABLE_GPGPU_PREEMPTION: u32 = 1 << 2;
pub const GEN9_DISABLE_GATHER_AT_SET_SHADER_COMMON_SLICE: u32 = 1 << 12;
pub const GEN9_CCS_TLB_PREFETCH_ENABLE: u32 = 1 << 3;
pub const GEN9_SAMPLER_HASH_COMPRESSED_READ_ADDR: u32 = 1 << 8;
pub const GEN10_RPM_CONFIG0_CTC_SHIFT_PARAMETER_MASK: u32 = 0x6;
pub const GEN11_COHERENT_PARTIAL_WRITE_MERGE_ENABLE: u32 = 1 << 19;
pub const GEN11_SAMPLER_ENABLE_HEADLESS_MSG: u32 = 1 << 5;
pub const GEN11_LSN_UNSLCVC_GAFS_HALF_CL2_MAXALLOC: u32 = 1 << 9;
pub const GEN11_LSN_UNSLCVC_GAFS_HALF_SF_MAXALLOC: u32 = 1 << 7;
pub const GEN11_LQSC_CLEAN_EVICT_DISABLE: u32 = 1 << 6;
pub const GEN11_INDIRECT_STATE_BASE_ADDR_OVERRIDE: u32 = 1;
pub const GEN11_HASH_CTRL_MASK: u32 = (0x3 << 12) | 0xf;
pub const GEN11_HASH_CTRL_EXCL_MASK: u32 = genmask(6, 0);
pub const GEN11_HASH_CTRL_EXCL_BIT0: u32 = 1;
pub const GEN11_HASH_CTRL_BIT4: u32 = 1 << 12;
pub const GEN11_HASH_CTRL_BIT0: u32 = 1;
pub const GEN11_GT_VEBOX_DISABLE_MASK: u32 = 0xf << 16;
pub const GEN11_GT_VDBOX_DISABLE_MASK: u32 = 0xff;
pub const GEN11_GRDOM_GSC: u32 = 1 << 21;
pub const GEN11_GRDOM_VECS4: u32 = 1 << 16;
pub const GEN11_GRDOM_VECS3: u32 = 1 << 15;
pub const GEN11_GRDOM_VECS2: u32 = 1 << 14;
pub const GEN11_GRDOM_VECS: u32 = 1 << 13;
pub const GEN11_GRDOM_MEDIA8: u32 = 1 << 12;
pub const GEN11_GRDOM_MEDIA7: u32 = 1 << 11;
pub const GEN11_GRDOM_MEDIA6: u32 = 1 << 10;
pub const GEN11_GRDOM_MEDIA5: u32 = 1 << 9;
pub const GEN11_GRDOM_MEDIA4: u32 = 1 << 8;
pub const GEN11_GRDOM_MEDIA3: u32 = 1 << 7;
pub const GEN11_GRDOM_MEDIA2: u32 = 1 << 6;
pub const GEN11_GRDOM_MEDIA: u32 = 1 << 5;
pub const GEN11_GRDOM_BLT: u32 = 1 << 2;
pub const GEN12_WAIT_FOR_EVENT_POWER_DOWN_DISABLE: u32 = 1 << 7;
pub const GEN12_RCU_MODE_CCS_ENABLE: u32 = 1;
pub const GEN12_PUSH_CONST_DEREF_HOLD_DIS: u32 = 1 << 8;
pub const GEN12_MAX_MSLICES: u32 = 4;
pub const GEN12_GRDOM_GSC: u32 = 1 << 21;
pub const GEN12_GFX_PREFETCH_DISABLE: u32 = 1 << 10;
pub const GEN12_DISABLE_TDL_PUSH: u32 = 1 << 9;
pub const GEN12_DISABLE_READ_SUPPRESSION: u32 = 1 << 15;
pub const GEN12_DISABLE_EARLY_READ: u32 = 1 << 14;
pub const GEN12_DISABLE_CPS_AWARE_COLOR_PIPE: u32 = 1 << 9;
pub const GEN12_CTX_CTRL_RUNALONE_MODE: u32 = 1 << 7;
pub const GEN12_BUS_HASH_CTL_BIT_EXC: u32 = 1 << 7;
pub const GEN12_STRICT_RAR_ENABLE: u32 = 1 << 23;
pub const GEN12_DOP_CLOCK_GATE_RENDER_ENABLE: u32 = 1 << 1;
pub const GEN12_FF_TESSELATION_DOP_GATE_DISABLE: u32 = 1 << 19;
pub const GEN7_MAX_PS_THREAD_DEP: u32 = 8 << 12;
pub const GEN7_WA_L3_CHICKEN_MODE: u32 = 0x2000_0000;
pub const GEN7_WA_FOR_GEN7_L3_CONTROL: u32 = 0x3c47_ff8c;
pub const GEN7_FF_VS_REF_CNT_FFME: u32 = 1 << 15;
pub const GEN7_DISABLE_SAMPLER_PREFETCH: u32 = 1 << 30;
pub const GEN7_CSC1_RHWO_OPT_DISABLE_IN_RCC: u32 = 1 << 10;
pub const GEN7_SBE_SS_CACHE_DISPATCH_PORT_SHARING_DISABLE: u32 = 1 << 4;
pub const GEN7_PSD_SINGLE_PORT_DISPATCH_ENABLE: u32 = 1 << 3;
pub const GEN7_FF_VS_SCHED_HW: u32 = 0;
pub const GEN7_FF_TS_SCHED_HW: u32 = 0;
pub const GEN7_FF_DS_SCHED_HW: u32 = 0;
pub const GEN7_FF_SCHED_MASK: u32 = 0x0007_7070;
pub const GEN6_GRDOM_RENDER: u32 = 1 << 1;
pub const GEN6_GRDOM_MEDIA: u32 = 1 << 2;
pub const GEN6_GRDOM_BLT: u32 = 1 << 3;
pub const GEN6_GRDOM_VECS: u32 = 1 << 4;
pub const GEN11_GRDOM_RENDER: u32 = GEN6_GRDOM_RENDER;
pub const GEN6_WIZ_HASHING_16x4: u32 = 1 << 9;
pub const GEN6_WIZ_HASHING_MASK: u32 = (1 << 9) | (1 << 7);

const fn genmask(high: u32, low: u32) -> u32 {
    (u32::MAX >> (31 - high)) & (u32::MAX << low)
}

#[allow(non_snake_case)]
pub const fn GEN8_MCR_SUBSLICE(subslice: u32) -> u32 {
    (subslice & 3) << 24
}
#[allow(non_snake_case)]
pub const GEN8_MCR_SUBSLICE_MASK: u32 = 3 << 24;
#[allow(non_snake_case)]
pub const fn GEN8_MCR_SLICE(slice: u32) -> u32 {
    (slice & 3) << 26
}
#[allow(non_snake_case)]
pub const GEN8_MCR_SLICE_MASK: u32 = 3 << 26;
#[allow(non_snake_case)]
pub const fn GEN11_MCR_SUBSLICE(subslice: u32) -> u32 {
    (subslice & 7) << 24
}
#[allow(non_snake_case)]
pub const GEN11_MCR_SUBSLICE_MASK: u32 = 7 << 24;
#[allow(non_snake_case)]
pub const fn GEN11_MCR_SLICE(slice: u32) -> u32 {
    (slice & 0xf) << 27
}
#[allow(non_snake_case)]
pub const GEN11_MCR_SLICE_MASK: u32 = 0xf << 27;
#[allow(non_snake_case)]
pub const fn GEN6_WIZ_HASHING(hi: u32, lo: u32) -> u32 {
    (hi << 9) | (lo << 7)
}
#[allow(non_snake_case)]
pub const fn GEN9_PREEMPT_GPGPU_LEVEL(hi: u32, lo: u32) -> u32 {
    (hi << 2) | (lo << 1)
}
#[allow(non_snake_case)]
pub const fn GEN7_CXT_POWER_SIZE(value: u32) -> u32 {
    (value >> 25) & 0x7f
}
#[allow(non_snake_case)]
pub const fn GEN7_CXT_RING_SIZE(value: u32) -> u32 {
    (value >> 22) & 0x7
}
#[allow(non_snake_case)]
pub const fn GEN7_CXT_RENDER_SIZE(value: u32) -> u32 {
    (value >> 16) & 0x3f
}
#[allow(non_snake_case)]
pub const fn GEN7_CXT_EXTENDED_SIZE(value: u32) -> u32 {
    (value >> 9) & 0x7f
}
#[allow(non_snake_case)]
pub const fn GEN7_CXT_GT1_SIZE(value: u32) -> u32 {
    (value >> 6) & 0x7
}
#[allow(non_snake_case)]
pub const fn GEN7_CXT_VFSTATE_SIZE(value: u32) -> u32 {
    value & 0x3f
}
#[allow(non_snake_case)]
pub const fn GEN7_CXT_TOTAL_SIZE(value: u32) -> u32 {
    GEN7_CXT_EXTENDED_SIZE(value) + GEN7_CXT_VFSTATE_SIZE(value)
}
#[allow(non_snake_case)]
pub const fn GEN6_CXT_POWER_SIZE(value: u32) -> u32 {
    (value >> 24) & 0x3f
}
#[allow(non_snake_case)]
pub const fn GEN6_CXT_RING_SIZE(value: u32) -> u32 {
    (value >> 18) & 0x3f
}
#[allow(non_snake_case)]
pub const fn GEN6_CXT_RENDER_SIZE(value: u32) -> u32 {
    (value >> 12) & 0x3f
}
#[allow(non_snake_case)]
pub const fn GEN6_CXT_EXTENDED_SIZE(value: u32) -> u32 {
    (value >> 6) & 0x3f
}
#[allow(non_snake_case)]
pub const fn GEN6_CXT_PIPELINE_SIZE(value: u32) -> u32 {
    value & 0x3f
}
#[allow(non_snake_case)]
pub const fn GEN6_CXT_TOTAL_SIZE(value: u32) -> u32 {
    GEN6_CXT_RING_SIZE(value) + GEN6_CXT_EXTENDED_SIZE(value) + GEN6_CXT_PIPELINE_SIZE(value)
}

// Linux drivers/gpu/drm/i915/gt/uc/abi/guc_klvs_abi.h and intel_guc.h.
pub const GUC_KLV_0_LEN: u32 = 0xffff;
pub const GUC_KLV_0_KEY: u32 = 0xffff << 16;
pub const fn MAKE_GUC_VER(major: u32, minor: u32, patch: u32) -> u32 {
    (major << 16) | (minor << 8) | patch
}

// Additional definitions used by the latest feature-check, copied from the
// corresponding Linux 7.2.3 GT/GEM headers. Keep register objects typed and
// encode firmware/control words at their documented 32-bit width.
pub const RPM_CONFIG0: I915Reg = mmio(0x0d00);
pub const MISC_STATUS0: I915Reg = mmio(0xa500);
pub const MISC_STATUS1: I915Reg = mmio(0xa504);
pub const CACHE_MODE_0: I915Reg = mmio(0x2120);
pub const CACHE_MODE_0_GEN7: I915Reg = mmio(0x7000);
pub const CACHE_MODE_1: I915Reg = mmio(0x7004);
pub const HDC_CHICKEN0: I915Reg = mmio(0x7300);
pub const HIZ_CHICKEN: I915Reg = mmio(0x7018);
pub const INSTPM: I915Reg = mmio(0x20c0);
pub const FF_SLICE_CHICKEN: I915Reg = mmio(0x2088);
pub const CXT_SIZE: I915Reg = mmio(0x21a0);
pub const HSW_PAVP_FUSE1: I915Reg = mmio(0x911c);
pub const DRAW_WATERMARK: I915Reg = mmio(0x26c0);
pub const IVB_FBC_RT_BASE: I915Reg = mmio(0x7020);
pub const IVB_FBC_RT_BASE_UPPER: I915Reg = mmio(0x7024);
pub const VF_PREEMPTION: I915Reg = mmio(0x83a4);
pub const PS_INVOCATION_COUNT: I915Reg = mmio(0x2348);
pub const HSW_SCRATCH1: I915Reg = mmio(0xb038);
pub const HSW_ROW_CHICKEN3: I915Reg = mmio(0xe49c);
pub const GAM_ECOCHK: I915Reg = mmio(0x4090);
pub const MMCD_MISC_CTRL: I915Reg = mmio(0x4ddc);
pub const GAMT_CHKN_BIT_REG: I915Reg = mmio(0x4ab8);
pub const MCFG_MCR_SELECTOR: I915Reg = mmio(0x0fd0);
pub const SF_MCR_SELECTOR: I915Reg = mmio(0x0fd8);
pub const GAM_MCR_SELECTOR: I915Reg = mmio(0x0fe0);
pub const UNSLICE_UNIT_LEVEL_CLKGATE: I915Reg = mmio(0x9434);
pub const UNSLICE_UNIT_LEVEL_CLKGATE2: I915Reg = mmio(0x94e4);
pub const SUBSLICE_UNIT_LEVEL_CLKGATE2: I915McrReg = mcr(0x9528);
pub const RENDER_MOD_CTRL: I915McrReg = mcr(0xcf2c);
pub const COMP_MOD_CTRL: I915McrReg = mcr(0xcf30);
pub const XEHP_VDBX_MOD_CTRL: I915McrReg = mcr(0xcf34);
pub const XEHP_VEBX_MOD_CTRL: I915McrReg = mcr(0xcf38);
pub const XEHP_GAMCNTRL_CTRL: I915McrReg = mcr(0xcf54);
pub const XEHP_L3NODEARBCFG: I915McrReg = mcr(0xb0b4);
pub const XEHP_L3SCQREG7: I915McrReg = mcr(0xb188);
pub const XEHP_SQCM: I915McrReg = mcr(0x8724);
pub const XEHP_HDC_CHICKEN0: I915McrReg = mcr(0xe5f0);
pub const HALF_SLICE_CHICKEN2: I915McrReg = mcr(0xe180);
pub const ICL_HDC_MODE: I915McrReg = mcr(0xe5f4);
pub const SARB_CHICKEN1: I915McrReg = mcr(0xe90c);
pub const CHICKEN_RASTER_2: I915McrReg = mcr(0x6208);
pub const VFLSKPD: I915McrReg = mcr(0x62a8);
pub const XEHP_FF_MODE2: I915McrReg = mcr(0x6604);
pub const XEHP_PSS_MODE2: I915McrReg = mcr(0x703c);
pub const XEHP_PSS_CHICKEN: I915McrReg = mcr(0x7044);
pub const XELPMP_GSC_TLB_INV_CR: I915Reg = mmio(0xcf04);
pub const XEHP_GFX_TLB_INV_CR: I915McrReg = mcr(0xced8);
pub const XEHP_VD_TLB_INV_CR: I915McrReg = mcr(0xcedc);
pub const XEHP_VE_TLB_INV_CR: I915McrReg = mcr(0xcee0);
pub const XEHP_BLT_TLB_INV_CR: I915McrReg = mcr(0xcee4);
pub const XEHP_COMPCTX_TLB_INV_CR: I915McrReg = mcr(0xcf04);
pub const LSC_CHICKEN_BIT_0: I915McrReg = mcr(0xe7c8);
pub const LSC_CHICKEN_BIT_0_UDW: I915McrReg = mcr(0xe7cc);
pub const XELPMP_GSC_MOD_CTRL: I915Reg = mmio(0xcf30);
pub const XELPMP_VDBX_MOD_CTRL: I915Reg = mmio(0xcf34);
pub const XEHP_CCS_MODE: I915Reg = mmio(0x14804);
pub const VFG_PREEMPTION_CHICKEN: I915Reg = mmio(0x83b4);
pub const FF_SLICE_CS_CHICKEN2: I915Reg = mmio(0x20e4);
pub const GFX_MODE: I915Reg = mmio(0x2520);

pub const GT_RENDER_USER_INTERRUPT: u32 = 1 << 0;
pub const INSTPM_FORCE_ORDERING: u32 = 1 << 7;
pub const GUC_SEM_INTR_ROUTE_TO_GUC: u32 = 1 << 31;
pub const GUC_SEM_INTR_ENABLE_ALL: u32 = 0xff;
pub const SLPC_CTX_FREQ_REQ_IS_COMPUTE: u32 = 1 << 28;
pub const INTEL_GUC_TLB_INVAL_TYPE_MASK: u32 = genmask(7, 0);
pub const INTEL_GUC_TLB_INVAL_MODE_MASK: u32 = genmask(11, 8);
pub const INTEL_GUC_TLB_INVAL_FLUSH_CACHE: u32 = 1 << 31;
pub const INTEL_GUC_TLB_INVAL_ENGINES: u32 = 0;
pub const INTEL_GUC_TLB_INVAL_GUC: u32 = 3;
pub const INTEL_GUC_TLB_INVAL_MODE_HEAVY: u32 = 0;
pub const INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_MASK: u32 = 0x0000_00ff;
pub const INTEL_GUC_STATE_CAPTURE_EVENT_STATUS_NOSPACE: u32 = 1;
pub const INTEL_GUC_ACTION_REGISTER_CONTEXT: u32 = 0x4502;
pub const INTEL_GUC_ACTION_DEREGISTER_CONTEXT: u32 = 0x4503;
pub const INTEL_GUC_ACTION_REGISTER_CONTEXT_MULTI_LRC: u32 = 0x4601;
pub const INTEL_GUC_ACTION_SET_ENG_UTIL_BUFF: u32 = 0x550a;
pub const INTEL_GUC_ACTION_V69_SET_CONTEXT_PRIORITY: u32 = 0x1005;
pub const INTEL_GUC_ACTION_V69_SET_CONTEXT_PREEMPTION_TIMEOUT: u32 = 0x1007;
pub const INTEL_GUC_ACTION_HOST2GUC_UPDATE_CONTEXT_POLICIES: u32 = 0x100b;
pub const INTEL_GUC_ACTION_TLB_INVALIDATION: u32 = 0x7000;
pub const INTEL_GUC_ACTION_LIMIT: u32 = 0x8006;
pub const GLOBAL_SCHEDULE_POLICY_RC_YIELD_DURATION: u32 = 100;
pub const GLOBAL_SCHEDULE_POLICY_RC_YIELD_RATIO: u32 = 50;
pub const CONTEXT_REDZONE: u32 = 0x5a;
pub const POISON_INUSE: u32 = 0x5a;
pub const ERROR_CSB: u32 = 1 << 31;
pub const ERROR_PREEMPT: u32 = 1 << 30;
pub const GT_CS_MASTER_ERROR_INTERRUPT: u32 = 1 << 3;
pub const GT_WAIT_SEMAPHORE_INTERRUPT: u32 = 1 << 11;
pub const GT_CONTEXT_SWITCH_INTERRUPT: u32 = 1 << 8;
pub const INTEL_CONTEXT_BANNED_PREEMPT_TIMEOUT_MS: u32 = 1;
pub const CORE_DUMP_FLAG_NONE: u32 = 0;
pub const INTEL_GT_SCRATCH_FIELD_COHERENTL3_WA: u32 = 256;
pub const VERT_WM_VAL: u32 = genmask(9, 0);
pub const INSTRUCTION_STATE_CACHE_INVALIDATE: u32 = 1 << 6;
pub const PIPE_CONTROL_INSTRUCTION_CACHE_INVALIDATE: u32 = 1 << 11;
pub const XY_FAST_COLOR_BLT_CMD: u32 = (2 << 29) | (0x44 << 22);
pub const XY_FAST_COLOR_BLT_MOCS_MASK: u32 = genmask(27, 21);
pub const DG2_PREDICATE_RESULT_BB: usize = 2048;
pub const DG2_PREDICATE_RESULT_WA: usize =
    crate::linux_config::PAGE_SIZE - core::mem::size_of::<u64>();
pub const PIPE_CONTROL_CS_STALL: u32 = 1 << 20;
pub const PIPE_CONTROL_DC_FLUSH_ENABLE: u32 = 1 << 5;
pub const PIPE_CONTROL_FLUSH_L3: u32 = 1 << 27;
pub const PIPE_CONTROL_STORE_DATA_INDEX: u32 = 1 << 21;
pub const PIPE_CONTROL_QW_WRITE: u32 = 1 << 14;
pub const RC_OP_FLUSH_ENABLE: u32 = 1;
pub const HIZ_RAW_STALL_OPT_DISABLE: u32 = 1 << 2;
pub const DISABLE_REPACKING_FOR_COMPRESSION: u32 = 1 << 15;
pub const HDC_DONOT_FETCH_MEM_WHEN_MASKED: u32 = 1 << 11;
pub const HDC_FORCE_NON_COHERENT: u32 = 1 << 4;
pub const DOP_CLOCK_GATING_DISABLE: u32 = 1;
pub const HDC_FENCE_DEST_SLM_DISABLE: u32 = 1 << 14;
pub const HDC_FORCE_CONTEXT_SAVE_RESTORE_NON_COHERENT: u32 = 1 << 5;
pub const CHV_HZ_8X8_MODE_IN_1X: u32 = 1 << 15;
pub const PIXEL_SUBSPAN_COLLECT_OPT_DISABLE: u32 = 1 << 6;
pub const HDC_FORCE_CSR_NON_COHERENT_OVR_DISABLE: u32 = 1 << 15;
pub const FLOAT_BLEND_OPTIMIZATION_ENABLE: u32 = 1 << 4;
pub const HZ_DEPTH_TEST_LE_GE_OPT_DISABLE: u32 = 1 << 13;
pub const DISABLE_TDC_LOAD_BALANCING_CALC: u32 = 1 << 6;
pub const WAIT_ON_DEPTH_STALL_DONE_DISABLE: u32 = 1 << 5;
pub const DG1_FLOAT_POINT_BLEND_OPT_STRICT_MODE_EN: u32 = 1 << 12;
pub const DG1_HZ_READ_SUPPRESSION_OPTIMIZATION_DISABLE: u32 = 1 << 14;
pub const MSC_MSAA_REODER_BUF_BYPASS_DISABLE: u32 = 1 << 14;
pub const PREEMPTION_VERTEX_COUNT: u32 = genmask(15, 0);
pub const SCOREBOARD_STALL_FLUSH_CONTROL: u32 = 1 << 5;
pub const FD_END_COLLECT: u32 = 1 << 5;
pub const VF_PREFETCH_TLB_DIS: u32 = 1 << 5;
pub const TGL_NESTED_BB_EN: u32 = 1 << 12;
pub const L3_PWM_TIMER_INIT_VAL_MASK: u32 = genmask(9, 0);
pub const FF_MODE2_TDS_TIMER_MASK: u32 = genmask(23, 16);
pub const FF_MODE2_TDS_TIMER_128: u32 = 4 << 16;
pub const FF_MODE2_GS_TIMER_224: u32 = 224 << 24;
pub const TBIMR_FAST_CLIP: u32 = 1 << 5;
pub const BLEND_FILL_CACHING_OPT_DIS: u32 = 1 << 3;
pub const EN_32B_ACCESS: u32 = 1 << 30;
pub const XEHP_SFC_ENABLE_MASK: u32 = genmask(27, 24);
pub const XEHP_RCU_MODE_FIXED_SLICE_CCS_MODE: u32 = 1 << 1;
pub const ENABLE_EU_COUNT_FOR_TDL_FLUSH: u32 = 1 << 10;
pub const SC_DISABLE_POWER_OPTIMIZATION_EBB: u32 = 1 << 9;
pub const DIS_ATOMIC_CHAINING_TYPED_WRITES: u32 = 1 << 3;
pub const FF_DOP_CLOCK_GATE_DISABLE: u32 = 1 << 1;
pub const ENABLE_SMALLPL: u32 = 1 << 15;
pub const HSW_SAMPLE_C_PERFORMANCE: u32 = 1 << 9;
pub const GFX_TLB_INVALIDATE_EXPLICIT: u32 = 1 << 13;
pub const GFX_REPLAY_MODE: u32 = 1 << 11;
pub const CM0_STC_EVICT_DISABLE_LRA_SNB: u32 = 1 << 5;
pub const VS_TIMER_DISPATCH: u32 = 1 << 6;
pub const ECO_CONSTANT_BUFFER_SR_DISABLE: u32 = 1 << 4;
pub const STACKID_CTRL: u32 = genmask(6, 5);
pub const STACKID_CTRL_512: u32 = 2 << 5;
pub const THREAD_EX_ARB_MODE: u32 = genmask(3, 2);
pub const THREAD_EX_ARB_MODE_RR_AFTER_DEP: u32 = 2 << 2;
pub const MTL_DISABLE_FIX_FOR_EOT_FLUSH: u32 = 1 << 9;
pub const XELPG_DISABLE_TDL_SVHS_GATING: u32 = 1 << 1;
pub const MTL_DISABLE_SAMPLER_SC_OOO: u32 = 1 << 3;
pub const DISABLE_PREFETCH_INTO_IC: u32 = 1 << 3;
pub const DISABLE_128B_EVICTION_COMMAND_UDW: u32 = 1 << 4;
pub const POLYGON_TRIFAN_LINELOOP_DISABLE: u32 = 1 << 4;
pub const DISABLE_D8_D16_COASLESCE: u32 = 1 << 30;
pub const XEHP_DIS_BBL_SYSPIPE: u32 = 1 << 11;
pub const DIS_CHAIN_2XSIMD8: u32 = 1 << 23;
pub const UGM_FRAGMENT_THRESHOLD_TO_3: u32 = 1 << 26;
pub const MAXREQS_PER_BANK: u32 = genmask(7, 5);
pub const FORCE_1_SUB_MESSAGE_PER_FRAGMENT: u32 = 1 << 15;
pub const ENABLE_PREFETCH_INTO_IC: u32 = 1 << 3;
pub const I915_VIDEO_CLASS_CAPABILITY_HEVC: u32 = 1;
pub const I915_VIDEO_AND_ENHANCE_CLASS_CAPABILITY_SFC: u32 = 1 << 1;
pub const I915_GEM_HWS_SEQNO_ADDR: usize = 0x40 * core::mem::size_of::<u32>();
pub const I915_GEM_HWS_GGTT_BIND_ADDR: usize = 0x46 * core::mem::size_of::<u32>();
pub const SZ_4K: usize = 4096;
pub const SZ_512K: usize = 512 * 1024;
pub const U32_MAX: u32 = u32::MAX;
pub const NOTIFY_DONE: i32 = 0;
pub const SINGLE_DEPTH_NESTING: i32 = 1;
pub const INTEL_LEGACY_32B_CONTEXT: u32 = 1;
pub const INTEL_LEGACY_64B_CONTEXT: u32 = 3;
pub const INTEL_CONTEXT_SCHEDULE_IN: i32 = 0;
pub const INTEL_CONTEXT_SCHEDULE_OUT: i32 = 1;
pub const INTEL_SUBMISSION_RING: i32 = 0;
pub const INTEL_SUBMISSION_ELSP: i32 = 1;
pub const INTEL_SUBMISSION_GUC: i32 = 2;
pub const GT_MEDIA: i32 = 2;
pub const I915_PRIORITY_DISPLAY: i32 = 1026;
pub const I915_CONTEXT_DEFAULT_PRIORITY: i32 = 0;
pub const I915_ENGINE_CLASS_INVALID: i32 = -1;
pub const I915_CACHE_NONE: u32 = 0;
pub const STALL_NONE: i32 = 0;
pub const STALL_REGISTER_CONTEXT: i32 = 1;
pub const STALL_MOVE_LRC_TAIL: i32 = 2;
pub const STALL_ADD_REQUEST: i32 = 3;
pub const EMIT_INVALIDATE: u32 = 1 << 0;
pub const EMIT_FLUSH: u32 = 1 << 1;
pub const EMIT_BARRIER: u32 = EMIT_INVALIDATE | EMIT_FLUSH;
pub const FORCE_VIRTUAL: u32 = 1;
pub const MSG_IDLE_CS: I915Reg = mmio(0x8000);
pub const MSG_IDLE_VCS0: I915Reg = mmio(0x8004);
pub const MSG_IDLE_VCS1: I915Reg = mmio(0x8008);
pub const MSG_IDLE_BCS: I915Reg = mmio(0x800c);
pub const MSG_IDLE_VECS0: I915Reg = mmio(0x8010);
pub const MSG_IDLE_VCS2: I915Reg = mmio(0x80c0);
pub const MSG_IDLE_VCS3: I915Reg = mmio(0x80c4);
pub const MSG_IDLE_VCS4: I915Reg = mmio(0x80c8);
pub const MSG_IDLE_VCS5: I915Reg = mmio(0x80cc);
pub const MSG_IDLE_VCS6: I915Reg = mmio(0x80d0);
pub const MSG_IDLE_VCS7: I915Reg = mmio(0x80d4);
pub const MSG_IDLE_VECS1: I915Reg = mmio(0x80d8);
pub const MSG_IDLE_VECS2: I915Reg = mmio(0x80dc);
pub const MSG_IDLE_VECS3: I915Reg = mmio(0x80e0);
pub const MSG_IDLE_FW_MASK: u32 = genmask(13, 9);
pub const MSG_IDLE_FW_SHIFT: u32 = 9;
pub const GFX_RUN_LIST_ENABLE: u32 = 1 << 15;
pub const PER_CTX_BB_FORCE: u32 = 1 << 2;
pub const PER_CTX_BB_VALID: u32 = 1;
pub const FF_SLICE_CHICKEN_CL_PROVOKING_VERTEX_FIX: u32 = 1 << 1;
pub const ILK_FBC_RT_VALID: u32 = 1;
pub const FLOW_CONTROL_ENABLE: u32 = 1 << 15;
pub const MSAA_OPTIMIZATION_REDUC_DISABLE: u32 = 1 << 11;
pub const L3SQ_URB_READ_CAM_MATCH_DISABLE: u32 = 1 << 27;
pub const VLV_B0_WA_L3SQCREG1_VALUE: u32 = 0x00d3_0000;
pub const HSW_SCRATCH1_L3_DATA_ATOMICS_DISABLE: u32 = 1 << 27;
pub const HSW_ROW_CHICKEN3_L3_GLOBAL_ATOMICS_DISABLE: u32 = 1 << 6;
pub const ECOCHK_DIS_TLB: u32 = 1 << 8;
pub const MMCD_PCLA: u32 = 1 << 31;
pub const MMCD_HOTSPOT_EN: u32 = 1 << 27;
pub const BDW_DISABLE_HDC_INVALIDATION: u32 = 1 << 25;
pub const GAMT_ECO_ENABLE_IN_PLACE_DECOMPRESS: u32 = 1 << 18;
pub const GAMT_CHKN_DISABLE_DYNAMIC_CREDIT_SHARING: u32 = 1 << 28;
pub const GAMW_ECO_DEV_CTX_RELOAD_DISABLE: u32 = 1 << 7;
pub const GAMT_CHKN_DISABLE_L3_COH_PIPE: u32 = 1 << 31;
pub const VSUNIT_CLKGATE_DIS: u32 = 1 << 3;
pub const HSUNIT_CLKGATE_DIS: u32 = 1 << 8;
pub const PSDUNIT_CLKGATE_DIS: u32 = 1 << 5;
pub const GWUNIT_CLKGATE_DIS: u32 = 1 << 16;
pub const L3_CLKGATE_DIS: u32 = 1 << 16;
pub const L3_CR2X_CLKGATE_DIS: u32 = 1 << 17;
pub const DFR_DISABLE: u32 = 1 << 9;
pub const IECPUNIT_CLKGATE_DIS: u32 = 1 << 22;
pub const CPSSUNIT_CLKGATE_DIS: u32 = 1 << 9;
pub const VSUNIT_CLKGATE_DIS_TGL: u32 = 1 << 19;
pub const CG3DDISCFEG_CLKGATE_DIS: u32 = 1 << 17;
pub const DSS_ROUTER_CLKGATE_DIS: u32 = 1 << 28;
pub const COMP_CKN_IN: u32 = genmask(30, 29);
pub const INVALIDATION_BROADCAST_MODE_DIS: u32 = 1 << 12;
pub const GLOBAL_INVALIDATION_MODE: u32 = 1 << 2;
pub const XEHP_LNESPARE: u32 = 1 << 19;
pub const MFXPIPE_CLKGATE_DIS: u32 = 1 << 3;
pub const CMD_CCTL_MOCS_MASK: u32 = genmask(13, 7) | genmask(6, 0);
pub const BLIT_CCTL_MASK: u32 = genmask(14, 8) | genmask(6, 0);
pub const CM0_PIPELINED_RENDER_FLUSH_DISABLE: u32 = 1 << 8;
pub const L3_PRIO_CREDITS_MASK: u32 = (0x1f << 19) | (0x1f << 14);
pub const EVICTION_PERF_FIX_ENABLE: u32 = 1 << 8;
pub const XEHPC_BCS1_RING_BASE: u32 = 0x3e0000;
pub const XEHPC_BCS2_RING_BASE: u32 = 0x3e2000;
pub const XEHPC_BCS3_RING_BASE: u32 = 0x3e4000;
pub const XEHPC_BCS4_RING_BASE: u32 = 0x3e6000;
pub const XEHPC_BCS5_RING_BASE: u32 = 0x3e8000;
pub const XEHPC_BCS6_RING_BASE: u32 = 0x3ea000;
pub const XEHPC_BCS7_RING_BASE: u32 = 0x3ec000;
pub const XEHPC_BCS8_RING_BASE: u32 = 0x3ee000;
pub const XEHP_BSD5_RING_BASE: u32 = 0x1e0000;
pub const XEHP_BSD6_RING_BASE: u32 = 0x1e4000;
pub const XEHP_BSD7_RING_BASE: u32 = 0x1f0000;
pub const XEHP_BSD8_RING_BASE: u32 = 0x1f4000;
pub const XEHP_VEBOX3_RING_BASE: u32 = 0x1e8000;
pub const XEHP_VEBOX4_RING_BASE: u32 = 0x1f8000;
pub const MTL_GSC_RING_BASE: u32 = 0x11a000;
pub const XEHPC_GRDOM_BLT1: u32 = 1 << 24;
pub const XEHPC_GRDOM_BLT2: u32 = 1 << 25;
pub const XEHPC_GRDOM_BLT3: u32 = 1 << 26;
pub const XEHPC_GRDOM_BLT4: u32 = 1 << 27;
pub const XEHPC_GRDOM_BLT5: u32 = 1 << 28;
pub const XEHPC_GRDOM_BLT6: u32 = 1 << 29;
pub const XEHPC_GRDOM_BLT7: u32 = 1 << 30;
pub const XEHPC_GRDOM_BLT8: u32 = 1 << 31;
pub const ASYNC_FLIP_PERF_DISABLE: u32 = 1 << 14;
pub const PARTIAL_INSTRUCTION_SHOOTDOWN_DISABLE: u32 = 1 << 8;
pub const STALL_DOP_GATING_DISABLE: u32 = 1 << 5;
pub const XEHP_L3SQCREG5: I915McrReg = mcr(0xb158);
pub const XEHP_SLICE_COMMON_ECO_CHICKEN1: I915McrReg = mcr(0x731c);
pub const CMD_3DSTATE_MESH_CONTROL: u32 = (0x3 << 29) | (0x3 << 27) | (0x77 << 16) | 0x3;
pub const XEHP_COMMON_SLICE_CHICKEN3: I915McrReg = mcr(0x7304);
pub const LSC_L1_FLUSH_CTL_3D_DATAPORT_FLUSH_EVENTS_MASK: u32 = genmask(13, 11);
pub const BDW_SCRATCH1: I915McrReg = mcr(0xb11c);
pub const HSW_HALF_SLICE_CHICKEN3: I915Reg = mmio(0xe184);
pub const XEHP_BLITTER_SCHEDULING_MODE_MASK: u32 = genmask(12, 11);
pub const XEHP_BLITTER_ROUND_ROBIN_MODE: u32 = 1 << 11;
pub const RT_CTRL: I915McrReg = mcr(0xe530);
pub const PIN_GLOBAL: u64 = 1 << 10;
pub const PIN_USER: u64 = 1 << 11;
pub const EXEC_OBJECT_WRITE: u64 = 1;
// These source enum values are only used as indexes into steering_table (no
// Rust field stores the enum), so use usize to match Rust array indexing.
pub const L3BANK: usize = 0;
pub const MSLICE: usize = 1;
pub const LNCF: usize = 2;
pub const GAM: usize = 3;
pub const DSS: usize = 4;
pub const OADDRM: usize = 5;
pub const INSTANCE0: usize = 6;
pub const NUM_STEERING_TYPES: usize = 7;
// Linux intel_uncore.h's forcewake domain enum has 16 IDs (render through GSC).
pub const FORCEWAKE_ALL: i32 = (1 << 16) - 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_command_and_ring_register_encodings_match_linux_headers() {
        assert_eq!(MI_NOOP, 0);
        assert_eq!(MI_ARB_ON_OFF | MI_ARB_ENABLE, 0x0400_0001);
        assert_eq!(MI_LOAD_REGISTER_IMM(1), 0x1100_0001);
        assert_eq!(RING_MI_MODE(RENDER_RING_BASE).reg, 0x0209c);
        assert_eq!(REG_FIELD_PREP(0x0000_ff00, 0x42), 0x4200);
        assert_eq!(REG_FIELD_GET(0x0000_ff00, 0x4200), 0x42);
    }

    #[test]
    fn gen12_lrc_slots_and_register_types_match_linux_headers() {
        assert_eq!(I915_GTT_PAGE_SIZE, 4096);
        assert_eq!(LRC_STATE_OFFSET, 8192);
        assert_eq!(LRC_PPHWSP_SCRATCH_ADDR, 0xd0);
        assert_eq!(CTX_RING_HEAD, 5);
        assert_eq!(CTX_RING_TAIL, 7);
        assert_eq!(CTX_RING_START, 9);
        assert_eq!(CTX_RING_CTL, 11);
        assert_eq!(GEN12_CTX_PRIORITY_HIGH, 0x400);
        assert_eq!(GEN12_CTX_PRIORITY_NORMAL, 0x200);
        assert_eq!(GEN12_CTX_PRIORITY_LOW, 0);

        let mmio: I915Reg = GEN8_CS_CHICKEN1;
        let mcr: I915McrReg = GEN8_L3SQCREG4;
        assert_eq!(mmio.reg, 0x2580);
        assert_eq!(mcr.reg, 0xb118);
        assert_eq!(RING_CONTEXT_STATUS_PTR(0x2000).reg, 0x23a0);
        assert_eq!(GEN8_RING_CS_GPR(0x2000, 2).reg, 0x2610);
        assert_eq!(GEN8_RING_PDP_UDW(0x2000, 3).reg, 0x228c);
        assert_eq!(GEN8_RING_PDP_LDW(0x2000, 3).reg, 0x2288);
        assert_eq!(RING_FORCE_TO_NONPRIV(0x2000, 4).reg, 0x24e0);
    }

    #[test]
    fn guc_version_encoding_matches_linux_intel_guc_header() {
        assert_eq!(MAKE_GUC_VER(1, 2, 3), 0x0001_0203);
        assert_eq!(GUC_KLV_0_KEY, 0xffff_0000);
        assert_eq!(GUC_KLV_0_LEN, 0x0000_ffff);
    }

    #[test]
    fn latest_gt_register_and_abi_values_match_linux_headers() {
        let rpm: I915Reg = RPM_CONFIG0;
        let mcr: I915McrReg = XEHP_L3SQCREG5;
        assert_eq!(rpm.reg, 0xd00);
        assert_eq!(mcr.reg, 0xb158);
        assert_eq!(MSG_IDLE_FW_MASK, 0x3e00);
        assert_eq!(XEHP_BLITTER_SCHEDULING_MODE_MASK, 0x1800);
        assert_eq!(XEHP_BLITTER_ROUND_ROBIN_MODE, 0x0800);
        assert_eq!(XEHPC_BCS8_RING_BASE, 0x3ee000);
        assert_eq!(INTEL_GUC_ACTION_REGISTER_CONTEXT, 0x4502);
        assert_eq!(INTEL_GUC_ACTION_TLB_INVALIDATION, 0x7000);
        assert_eq!(FORCEWAKE_ALL, 0xffff);
        assert_eq!(NUM_STEERING_TYPES, 7);
    }
}
