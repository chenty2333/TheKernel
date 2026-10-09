// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Linux 7.2.3 `drivers/gpu/drm/i915/intel_pcode.c` and `intel_pcode.h`.
//! PCODE MMIO, uncore serialization, wait/polling, runtime PM, preemption, and
//! diagnostics are explicit backend operations; this module has no default
//! backend and does not assume successful mailbox access.
#![allow(dead_code, non_camel_case_types, clippy::too_many_arguments)]

// Public command, parameter, and bit-field definitions from
// include/drm/intel/intel_pcode_regs.h. `_MMIO(x)` constants are represented
// by their byte offsets; the two payload offsets are from i915_reg.h.
pub const GEN6_PCODE_MAILBOX: u32 = 0x138124;
pub const GEN6_PCODE_DATA: u32 = 0x138128;
pub const GEN6_PCODE_DATA1: u32 = 0x13812c;
pub const GEN6_PCODE_READY: u32 = 1 << 31;
pub const GEN6_PCODE_MB_PARAM2: u32 = 0x00ff_0000;
pub const GEN6_PCODE_MB_PARAM1: u32 = 0x0000_ff00;
pub const GEN6_PCODE_MB_COMMAND: u32 = 0x0000_00ff;
pub const GEN6_PCODE_ERROR_MASK: u32 = 0x0000_00ff;
pub const GEN6_PCODE_SUCCESS: u32 = 0x00;
pub const GEN6_PCODE_ILLEGAL_CMD: u32 = 0x01;
pub const GEN6_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE: u32 = 0x02;
pub const GEN6_PCODE_TIMEOUT: u32 = 0x03;
pub const GEN6_PCODE_UNIMPLEMENTED_CMD: u32 = 0xff;
pub const GEN7_PCODE_TIMEOUT: u32 = 0x02;
pub const GEN7_PCODE_ILLEGAL_DATA: u32 = 0x03;
pub const GEN11_PCODE_ILLEGAL_SUBCOMMAND: u32 = 0x04;
pub const GEN11_PCODE_LOCKED: u32 = 0x06;
pub const GEN11_PCODE_REJECTED: u32 = 0x11;
pub const GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE: u32 = 0x10;
pub const GEN6_PCODE_WRITE_RC6VIDS: u32 = 0x04;
pub const GEN6_PCODE_READ_RC6VIDS: u32 = 0x05;
pub const BDW_PCODE_DISPLAY_FREQ_CHANGE_REQ: u32 = 0x18;
pub const GEN9_PCODE_READ_MEM_LATENCY: u32 = 0x06;
pub const GEN9_MEM_LATENCY_LEVEL_3_7_MASK: u32 = 0xff00_0000;
pub const GEN9_MEM_LATENCY_LEVEL_2_6_MASK: u32 = 0x00ff_0000;
pub const GEN9_MEM_LATENCY_LEVEL_1_5_MASK: u32 = 0x0000_ff00;
pub const GEN9_MEM_LATENCY_LEVEL_0_4_MASK: u32 = 0x0000_00ff;
pub const SKL_PCODE_LOAD_HDCP_KEYS: u32 = 0x05;
pub const SKL_PCODE_CDCLK_CONTROL: u32 = 0x07;
pub const SKL_CDCLK_PREPARE_FOR_CHANGE: u32 = 0x03;
pub const SKL_CDCLK_READY_FOR_CHANGE: u32 = 0x01;
pub const GEN6_PCODE_WRITE_MIN_FREQ_TABLE: u32 = 0x08;
pub const GEN6_PCODE_READ_MIN_FREQ_TABLE: u32 = 0x09;
pub const GEN6_READ_OC_PARAMS: u32 = 0x0c;
pub const ICL_PCODE_MEM_SUBSYSYSTEM_INFO: u32 = 0x0d;
pub const ICL_PCODE_MEM_SS_READ_GLOBAL_INFO: u32 = 0x0000;
pub const DISPLAY_TO_PCODE_CDCLK_MAX: u32 = 0x28d;
pub const DISPLAY_TO_PCODE_VOLTAGE_MASK: u32 = 0x0000_0003;
pub const DISPLAY_TO_PCODE_VOLTAGE_MAX: u32 = DISPLAY_TO_PCODE_VOLTAGE_MASK;
pub const DISPLAY_TO_PCODE_CDCLK_VALID: u32 = 1 << 27;
pub const DISPLAY_TO_PCODE_PIPE_COUNT_VALID: u32 = 1 << 31;
pub const DISPLAY_TO_PCODE_CDCLK_MASK: u32 = 0x03ff_0000;
pub const DISPLAY_TO_PCODE_PIPE_COUNT_MASK: u32 = 0x7000_0000;
pub const ICL_PCODE_SAGV_DE_MEM_SS_CONFIG: u32 = 0x0e;
pub const ICL_PCODE_REP_QGV_MASK: u32 = 0x0000_0003;
pub const ICL_PCODE_REP_QGV_SAFE: u32 = 0x0000_0000;
pub const ICL_PCODE_REP_QGV_POLL: u32 = 0x0000_0001;
pub const ICL_PCODE_REP_QGV_REJECTED: u32 = 0x0000_0002;
pub const ADLS_PCODE_REP_PSF_MASK: u32 = 0x0000_000c;
pub const ADLS_PCODE_REP_PSF_SAFE: u32 = 0x0000_0000;
pub const ADLS_PCODE_REP_PSF_POLL: u32 = 0x0000_0004;
pub const ADLS_PCODE_REP_PSF_REJECTED: u32 = 0x0000_0008;
pub const ICL_PCODE_REQ_QGV_PT_MASK: u32 = 0x0000_00ff;
pub const ADLS_PCODE_REQ_PSF_PT_MASK: u32 = 0x0000_0700;
pub const GEN6_PCODE_READ_D_COMP: u32 = 0x10;
pub const GEN6_PCODE_WRITE_D_COMP: u32 = 0x11;
pub const ICL_PCODE_EXIT_TCCOLD: u32 = 0x12;
pub const HSW_PCODE_DE_WRITE_FREQ_REQ: u32 = 0x17;
pub const DISPLAY_IPS_CONTROL: u32 = 0x19;
pub const TGL_PCODE_TCCOLD: u32 = 0x26;
pub const TGL_PCODE_EXIT_TCCOLD_DATA_L_EXIT_FAILED: u32 = 1 << 0;
pub const TGL_PCODE_EXIT_TCCOLD_DATA_L_BLOCK_REQ: u32 = 0;
pub const TGL_PCODE_EXIT_TCCOLD_DATA_L_UNBLOCK_REQ: u32 = 1 << 0;
pub const IPS_PCODE_CONTROL: u32 = 1 << 30;
pub const HSW_PCODE_DYNAMIC_DUTY_CYCLE_CONTROL: u32 = 0x1a;
pub const GEN9_PCODE_SAGV_CONTROL: u32 = 0x21;
pub const GEN9_SAGV_DISABLE: u32 = 0x00;
pub const GEN9_SAGV_IS_DISABLED: u32 = 0x01;
pub const GEN9_SAGV_ENABLE: u32 = 0x03;
pub const DG1_PCODE_STATUS: u32 = 0x7e;
pub const DG1_UNCORE_GET_INIT_STATUS: u32 = 0x00;
pub const DG1_UNCORE_INIT_STATUS_COMPLETE: u32 = 0x01;
pub const PCODE_POWER_SETUP: u32 = 0x7c;
pub const POWER_SETUP_SUBCOMMAND_READ_I1: u32 = 0x04;
pub const POWER_SETUP_SUBCOMMAND_WRITE_I1: u32 = 0x05;
pub const POWER_SETUP_I1_WATTS: u32 = 1 << 31;
pub const POWER_SETUP_I1_SHIFT: u32 = 6;
pub const POWER_SETUP_I1_DATA_MASK: u32 = 0x0000_ffff;
pub const POWER_SETUP_SUBCOMMAND_G8_ENABLE: u32 = 0x06;
pub const GEN12_PCODE_READ_SAGV_BLOCK_TIME_US: u32 = 0x23;
pub const XEHP_PCODE_FREQUENCY_CONFIG: u32 = 0x6e;
pub const PCODE_MBOX_FC_SC_READ_FUSED_P0: u32 = 0x00;
pub const PCODE_MBOX_FC_SC_READ_FUSED_PN: u32 = 0x01;
pub const PCODE_MBOX_DOMAIN_NONE: u32 = 0x00;
pub const PCODE_MBOX_DOMAIN_MEDIAFF: u32 = 0x03;

#[allow(non_snake_case)]
pub const fn GEN6_ENCODE_RC6_VID(mv: u32) -> u32 { mv.wrapping_sub(245) / 5 }
#[allow(non_snake_case)]
pub const fn GEN6_DECODE_RC6_VID(vids: u32) -> u32 { vids * 5 + 245 }
#[allow(non_snake_case)]
pub const fn ICL_PCODE_MEM_SS_READ_QGV_POINT_INFO(point: u32) -> u32 {
    (point << 16) | (0x1 << 8)
}
pub const ADL_PCODE_MEM_SS_READ_PSF_GV_INFO: u32 = (0) | (0x2 << 8);
#[allow(non_snake_case)]
pub const fn DISPLAY_TO_PCODE_CDCLK(x: u32) -> u32 { (x << 16) & DISPLAY_TO_PCODE_CDCLK_MASK }
#[allow(non_snake_case)]
pub const fn DISPLAY_TO_PCODE_PIPE_COUNT(x: u32) -> u32 { (x << 28) & DISPLAY_TO_PCODE_PIPE_COUNT_MASK }
#[allow(non_snake_case)]
pub const fn DISPLAY_TO_PCODE_VOLTAGE(x: u32) -> u32 { x & DISPLAY_TO_PCODE_VOLTAGE_MASK }
#[allow(non_snake_case)]
pub const fn DISPLAY_TO_PCODE_UPDATE_MASK(cdclk: u32, num_pipes: u32, voltage_level: u32) -> u32 {
    DISPLAY_TO_PCODE_CDCLK(cdclk)
        | DISPLAY_TO_PCODE_PIPE_COUNT(num_pipes)
        | DISPLAY_TO_PCODE_VOLTAGE(voltage_level)
}
#[allow(non_snake_case)]
pub const fn ICL_PCODE_REQ_QGV_PT(x: u32) -> u32 { x & ICL_PCODE_REQ_QGV_PT_MASK }
#[allow(non_snake_case)]
pub const fn ADLS_PCODE_REQ_PSF_PT(x: u32) -> u32 { (x << 8) & ADLS_PCODE_REQ_PSF_PT_MASK }

// Linux errno values are positive constants in C and negated at return sites.
pub const EAGAIN: i32 = 11;
pub const ENODEV: i32 = 19;
pub const ENXIO: i32 = 6;
pub const EOVERFLOW: i32 = 75;
pub const ETIMEDOUT: i32 = 110;
pub const EINVAL: i32 = 22;
pub const EBUSY: i32 = 16;
pub const EACCES: i32 = 13;
pub const EPROBE_DEFER: i32 = 517;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcodeLogEvent {
    MissingStatusCase(u32),
    ReadFailure { mailbox: u32, error: i32 },
    WriteFailure { mailbox: u32, value: u32, error: i32 },
    RetryWithPreemptionDisabled,
    TimeoutBaseExceedsExpected(u32),
    WaitingForHardwareInitialization,
}

/// Required kernel/uncore seam. Implementations provide real device semantics;
/// there is intentionally no no-op or successful default implementation.
pub trait IntelPcodeBackend {
    fn lock_sb(&mut self);
    fn unlock_sb(&mut self);
    /// Equivalent of i915 `lockdep_assert_held(&uncore->i915->sb_lock)`.
    fn lockdep_assert_held(&self);
    fn read32_fw(&mut self, register: u32) -> u32;
    fn write32_fw(&mut self, register: u32, value: u32);
    /// Return true on timeout, matching `__intel_wait_for_register_fw()`'s use.
    /// On completion, store the final register value when `last_value` is Some.
    fn wait_for_register_fw(
        &mut self,
        register: u32,
        mask: u32,
        expected: u32,
        fast_timeout_us: i32,
        slow_timeout_ms: i32,
        last_value: Option<&mut u32>,
    ) -> bool;
    fn graphics_ver(&self) -> u32;
    fn is_dgfx(&self) -> bool;
    fn log(&mut self, event: PcodeLogEvent);
    /// Equivalent of `drm_WARN_ON_ONCE()` at the PCODE retry call site.
    fn warn_on_once_timeout_base(&mut self, timeout_base_ms: i32);
    fn preempt_disable(&mut self);
    fn preempt_enable(&mut self);
    /// Linux `_wait_for(condition, timeout_us, 10, 10)` equivalent. 0 means
    /// condition became true; nonzero preserves the kernel wait's timeout code.
    fn wait_for<F>(&mut self, condition: F, timeout_us: i32, min_us: i32, max_us: i32) -> i32
    where
        F: FnMut(&mut Self) -> bool;
    /// Linux `wait_for_atomic(condition, timeout_ms)` equivalent.
    fn wait_for_atomic<F>(&mut self, condition: F, timeout_ms: i32) -> i32
    where
        F: FnMut(&mut Self) -> bool;
    fn runtime_pm_get(&mut self);
    fn runtime_pm_put(&mut self);
}

// upstream: intel_pcode.c gen6_check_mailbox_status()
fn gen6_check_mailbox_status<B: IntelPcodeBackend>(backend: &mut B, mbox: u32) -> i32 {
    match mbox & GEN6_PCODE_ERROR_MASK {
        GEN6_PCODE_SUCCESS => 0,
        GEN6_PCODE_UNIMPLEMENTED_CMD => -ENODEV,
        GEN6_PCODE_ILLEGAL_CMD => -ENXIO,
        GEN6_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE
        | GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE => -EOVERFLOW,
        GEN6_PCODE_TIMEOUT => -ETIMEDOUT,
        _ => {
            backend.log(PcodeLogEvent::MissingStatusCase(mbox & GEN6_PCODE_ERROR_MASK));
            0
        }
    }
}

// upstream: intel_pcode.c gen7_check_mailbox_status()
fn gen7_check_mailbox_status<B: IntelPcodeBackend>(backend: &mut B, mbox: u32) -> i32 {
    match mbox & GEN6_PCODE_ERROR_MASK {
        GEN6_PCODE_SUCCESS => 0,
        GEN6_PCODE_ILLEGAL_CMD => -ENXIO,
        GEN7_PCODE_TIMEOUT => -ETIMEDOUT,
        GEN7_PCODE_ILLEGAL_DATA => -EINVAL,
        GEN11_PCODE_ILLEGAL_SUBCOMMAND => -ENXIO,
        GEN11_PCODE_LOCKED => -EBUSY,
        GEN11_PCODE_REJECTED => -EACCES,
        GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE => -EOVERFLOW,
        _ => {
            backend.log(PcodeLogEvent::MissingStatusCase(mbox & GEN6_PCODE_ERROR_MASK));
            0
        }
    }
}

// upstream: intel_pcode.c __snb_pcode_rw()
fn __snb_pcode_rw<B: IntelPcodeBackend>(
    backend: &mut B,
    mut mbox: u32,
    val: &mut u32,
    val1: Option<&mut u32>,
    fast_timeout_us: i32,
    slow_timeout_ms: i32,
    is_read: bool,
) -> i32 {
    backend.lockdep_assert_held();
    let value1 = match val1.as_ref() { Some(value) => **value, None => 0 };
    if backend.read32_fw(GEN6_PCODE_MAILBOX) & GEN6_PCODE_READY != 0 {
        return -EAGAIN;
    }

    backend.write32_fw(GEN6_PCODE_DATA, *val);
    backend.write32_fw(GEN6_PCODE_DATA1, value1);
    backend.write32_fw(GEN6_PCODE_MAILBOX, GEN6_PCODE_READY | mbox);

    if backend.wait_for_register_fw(
        GEN6_PCODE_MAILBOX,
        GEN6_PCODE_READY,
        0,
        fast_timeout_us,
        slow_timeout_ms,
        Some(&mut mbox),
    ) {
        return -ETIMEDOUT;
    }

    if is_read {
        *val = backend.read32_fw(GEN6_PCODE_DATA);
    }
    if is_read {
        if let Some(value) = val1 {
            *value = backend.read32_fw(GEN6_PCODE_DATA1);
        }
    }

    if backend.graphics_ver() > 6 {
        gen7_check_mailbox_status(backend, mbox)
    } else {
        gen6_check_mailbox_status(backend, mbox)
    }
}

// upstream: intel_pcode.c snb_pcode_read()
pub fn snb_pcode_read<B: IntelPcodeBackend>(
    backend: &mut B,
    mbox: u32,
    val: &mut u32,
    val1: Option<&mut u32>,
) -> i32 {
    backend.lock_sb();
    let err = __snb_pcode_rw(backend, mbox, val, val1, 500, 20, true);
    backend.unlock_sb();

    if err != 0 {
        // Linux additionally prints `%ps` for the caller return address; Rust
        // has no portable equivalent, while the event and original diagnostics remain.
        backend.log(PcodeLogEvent::ReadFailure { mailbox: mbox, error: err });
    }

    err
}

// upstream: intel_pcode.c snb_pcode_write_timeout()
pub fn snb_pcode_write_timeout<B: IntelPcodeBackend>(
    backend: &mut B,
    mbox: u32,
    val: u32,
    timeout_ms: i32,
) -> i32 {
    let mut value = val;
    backend.lock_sb();
    let err = __snb_pcode_rw(backend, mbox, &mut value, None, 250, timeout_ms, false);
    backend.unlock_sb();

    if err != 0 {
        backend.log(PcodeLogEvent::WriteFailure { mailbox: mbox, value, error: err });
    }

    err
}

// upstream: intel_pcode.c skl_pcode_try_request()
fn skl_pcode_try_request<B: IntelPcodeBackend>(
    backend: &mut B,
    mbox: u32,
    request: u32,
    reply_mask: u32,
    reply: u32,
    status: &mut u32,
) -> bool {
    let mut request = request;
    *status = __snb_pcode_rw(backend, mbox, &mut request, None, 500, 0, true) as u32;

    (*status == 0) && ((request & reply_mask) == reply)
}

/**
 * Linux v7.2.3: send PCODE request until acknowledgment.
 * Poll for timeout_base_ms with preemption, then another 50 ms without it.
 */
// upstream: intel_pcode.c skl_pcode_request()
pub fn skl_pcode_request<B: IntelPcodeBackend>(
    backend: &mut B,
    mbox: u32,
    request: u32,
    reply_mask: u32,
    reply: u32,
    timeout_base_ms: i32,
) -> i32 {
    let mut status: u32 = 0;
    let ret: i32;

    backend.lock_sb();

    // Prime PCODE explicitly because `_wait_for()` does not guarantee when its
    // first condition evaluation occurs.
    if skl_pcode_try_request(backend, mbox, request, reply_mask, reply, &mut status) {
        ret = 0;
    } else {
        let first_ret = backend.wait_for(
            |backend| skl_pcode_try_request(backend, mbox, request, reply_mask, reply, &mut status),
            timeout_base_ms * 1000,
            10,
            10,
        );
        if first_ret == 0 {
            ret = first_ret;
        } else {
            backend.log(PcodeLogEvent::RetryWithPreemptionDisabled);
            if timeout_base_ms > 3 {
                backend.warn_on_once_timeout_base(timeout_base_ms);
            }
            backend.preempt_disable();
            ret = backend.wait_for_atomic(
                |backend| skl_pcode_try_request(backend, mbox, request, reply_mask, reply, &mut status),
                50,
            );
            backend.preempt_enable();
        }
    }

    backend.unlock_sb();
    if status != 0 { status as i32 } else { ret }
}

// upstream: intel_pcode.c pcode_init_wait()
fn pcode_init_wait<B: IntelPcodeBackend>(backend: &mut B, timeout_ms: i32) -> i32 {
    if backend.wait_for_register_fw(
        GEN6_PCODE_MAILBOX,
        GEN6_PCODE_READY,
        0,
        500,
        timeout_ms,
        None,
    ) {
        return -EPROBE_DEFER;
    }

    skl_pcode_request(
        backend,
        DG1_PCODE_STATUS,
        DG1_UNCORE_GET_INIT_STATUS,
        DG1_UNCORE_INIT_STATUS_COMPLETE,
        DG1_UNCORE_INIT_STATUS_COMPLETE,
        timeout_ms,
    )
}

// upstream: intel_pcode.c intel_pcode_init()
pub fn intel_pcode_init<B: IntelPcodeBackend>(backend: &mut B) -> i32 {
    if !backend.is_dgfx() {
        return 0;
    }

    // Wait 10 seconds for the punit to settle and complete transactions.
    let mut err = pcode_init_wait(backend, 10_000);
    if err != 0 {
        backend.log(PcodeLogEvent::WaitingForHardwareInitialization);
        err = pcode_init_wait(backend, 180_000);
    }

    err
}

// upstream: intel_pcode.c snb_pcode_read_p()
pub fn snb_pcode_read_p<B: IntelPcodeBackend>(
    backend: &mut B,
    mbcmd: u32,
    p1: u32,
    p2: u32,
    val: &mut u32,
) -> i32 {
    let mbox = ((mbcmd << 0) & GEN6_PCODE_MB_COMMAND)
        | ((p1 << 8) & GEN6_PCODE_MB_PARAM1)
        | ((p2 << 16) & GEN6_PCODE_MB_PARAM2);
    backend.runtime_pm_get();
    let err = snb_pcode_read(backend, mbox, val, None);
    backend.runtime_pm_put();
    err
}

// upstream: intel_pcode.c snb_pcode_write_p()
pub fn snb_pcode_write_p<B: IntelPcodeBackend>(
    backend: &mut B,
    mbcmd: u32,
    p1: u32,
    p2: u32,
    val: u32,
) -> i32 {
    let mbox = ((mbcmd << 0) & GEN6_PCODE_MB_COMMAND)
        | ((p1 << 8) & GEN6_PCODE_MB_PARAM1)
        | ((p2 << 16) & GEN6_PCODE_MB_PARAM2);
    backend.runtime_pm_get();
    let err = snb_pcode_write(backend, mbox, val);
    backend.runtime_pm_put();
    err
}

// upstream: intel_pcode.c intel_pcode_read()
fn intel_pcode_read<B: IntelPcodeBackend>(backend: &mut B, mbox: u32, val: &mut u32, val1: Option<&mut u32>) -> i32 {
    snb_pcode_read(backend, mbox, val, val1)
}

// upstream: intel_pcode.c intel_pcode_write_timeout()
fn intel_pcode_write_timeout<B: IntelPcodeBackend>(backend: &mut B, mbox: u32, val: u32, timeout_ms: i32) -> i32 {
    snb_pcode_write_timeout(backend, mbox, val, timeout_ms)
}

// upstream: intel_pcode.c intel_pcode_request()
fn intel_pcode_request<B: IntelPcodeBackend>(
    backend: &mut B,
    mbox: u32,
    request: u32,
    reply_mask: u32,
    reply: u32,
    timeout_base_ms: i32,
) -> i32 {
    skl_pcode_request(backend, mbox, request, reply_mask, reply, timeout_base_ms)
}

/// Rust facade corresponding to `i915_display_pcode_interface` from the source.
/// The caller supplies an explicit backend for each operation.
pub struct IntelDisplayPcodeInterface;
#[allow(non_upper_case_globals)]
pub const i915_display_pcode_interface: IntelDisplayPcodeInterface = IntelDisplayPcodeInterface;
impl IntelDisplayPcodeInterface {
    pub fn read<B: IntelPcodeBackend>(&self, backend: &mut B, mbox: u32, val: &mut u32, val1: Option<&mut u32>) -> i32 {
        intel_pcode_read(backend, mbox, val, val1)
    }
    pub fn write<B: IntelPcodeBackend>(&self, backend: &mut B, mbox: u32, val: u32, timeout_ms: i32) -> i32 {
        intel_pcode_write_timeout(backend, mbox, val, timeout_ms)
    }
    pub fn request<B: IntelPcodeBackend>(&self, backend: &mut B, mbox: u32, request: u32, reply_mask: u32, reply: u32, timeout_base_ms: i32) -> i32 {
        intel_pcode_request(backend, mbox, request, reply_mask, reply, timeout_base_ms)
    }
}

/// Inline-macro equivalent from `intel_pcode.h` (default write timeout is 1 ms).
pub fn snb_pcode_write<B: IntelPcodeBackend>(backend: &mut B, mbox: u32, val: u32) -> i32 {
    snb_pcode_write_timeout(backend, mbox, val, 1)
}
