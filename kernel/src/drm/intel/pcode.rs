//! N305's active PCode mailbox adapter.
//!
//! The algorithm and status mapping are the MIT `intel_pcode.c` translation in
//! `tk-intel-display::intel_pcode_full`. This adapter supplies the kernel's
//! typed register window, southbridge serialization, bounded waits, and
//! explicit unsupported-runtime-PM boundary. The current kernel keeps the
//! integrated display device in D0 for its lifetime; the PCode `*_p` wrappers
//! are deliberately rejected because this tree has no PCI runtime-PM owner.

use core::sync::atomic::{AtomicBool, Ordering};

use intel_display::intel_pcode_full::{self, IntelPcodeBackend, PcodeLogEvent};
use kernel_guard::NoPreempt;
use spin::{Mutex, MutexGuard};

use super::{
    gmbus::PollTimer,
    regs::{self, Registers},
};

static PCODE_SB_LOCK: Mutex<()> = Mutex::new(());
static WARNED_LONG_TIMEOUT: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PcodeError {
    Unreadable(u32),
    WriteRefused(u32),
    LockInvariant,
    RuntimePmUnavailable,
    MailboxStatus(i32),
}

/// A backend is scoped to one live MMIO window and its monotonic timer.
pub(crate) struct RegisterPcode<'a, R, T> {
    regs: &'a R,
    timer: &'a T,
    graphics_ver: u32,
    dgfx: bool,
    sb_guard: Option<MutexGuard<'static, ()>>,
    preempt_guard: Option<NoPreempt>,
    failure: Option<PcodeError>,
}

impl<'a, R: Registers, T: PollTimer> RegisterPcode<'a, R, T> {
    pub(crate) fn new(regs: &'a R, timer: &'a T, graphics_ver: u32, dgfx: bool) -> Self {
        Self {
            regs,
            timer,
            graphics_ver,
            dgfx,
            sb_guard: None,
            preempt_guard: None,
            failure: None,
        }
    }

    fn register(offset: u32) -> Option<regs::Register> {
        match offset {
            intel_pcode_full::GEN6_PCODE_MAILBOX => Some(regs::pcode::GEN6_PCODE_MAILBOX),
            intel_pcode_full::GEN6_PCODE_DATA => Some(regs::pcode::GEN6_PCODE_DATA),
            intel_pcode_full::GEN6_PCODE_DATA1 => Some(regs::pcode::GEN6_PCODE_DATA1),
            _ => None,
        }
    }

    fn read(&mut self, offset: u32) -> u32 {
        if self.failure.is_some() {
            return u32::MAX;
        }
        let Some(register) = Self::register(offset) else {
            self.failure = Some(PcodeError::Unreadable(offset));
            return u32::MAX;
        };
        match self.regs.read(register) {
            Some(value) if value != u32::MAX => value,
            _ => {
                self.failure = Some(PcodeError::Unreadable(offset));
                u32::MAX
            }
        }
    }

    fn write(&mut self, offset: u32, value: u32) {
        if self.failure.is_some() {
            return;
        }
        let Some(register) = Self::register(offset) else {
            self.failure = Some(PcodeError::WriteRefused(offset));
            return;
        };
        if !self.regs.write(register, value) {
            self.failure = Some(PcodeError::WriteRefused(offset));
        }
    }

    fn pause(&self) {
        self.timer.pause();
    }

    fn result(&mut self, status: i32) -> Result<(), PcodeError> {
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        if status != 0 {
            return Err(PcodeError::MailboxStatus(status));
        }
        Ok(())
    }
}

impl<R: Registers, T: PollTimer> IntelPcodeBackend for RegisterPcode<'_, R, T> {
    fn lock_sb(&mut self) {
        if self.sb_guard.is_some() {
            self.failure = Some(PcodeError::LockInvariant);
            return;
        }
        self.sb_guard = Some(PCODE_SB_LOCK.lock());
    }

    fn unlock_sb(&mut self) {
        if self.sb_guard.take().is_none() {
            self.failure = Some(PcodeError::LockInvariant);
        }
    }

    fn lockdep_assert_held(&self) {
        assert!(self.sb_guard.is_some(), "PCode mailbox lock must be held");
    }

    fn read32_fw(&mut self, register: u32) -> u32 {
        self.read(register)
    }

    fn write32_fw(&mut self, register: u32, value: u32) {
        self.write(register, value)
    }

    fn wait_for_register_fw(
        &mut self,
        register: u32,
        mask: u32,
        expected: u32,
        fast_timeout_us: i32,
        slow_timeout_ms: i32,
        last_value: Option<&mut u32>,
    ) -> bool {
        let budget_us = u64::try_from(fast_timeout_us.max(0))
            .unwrap_or(0)
            .saturating_add(u64::try_from(slow_timeout_ms.max(0)).unwrap_or(0) * 1_000);
        let start = self.timer.now_micros();
        loop {
            let value = self.read(register);
            if value == u32::MAX && self.failure.is_some() {
                return true;
            }
            if value & mask == expected {
                if let Some(last_value) = last_value {
                    *last_value = value;
                }
                return false;
            }
            if self.timer.now_micros().saturating_sub(start) >= budget_us {
                if let Some(last_value) = last_value {
                    *last_value = value;
                }
                return true;
            }
            self.pause();
        }
    }

    fn graphics_ver(&self) -> u32 {
        self.graphics_ver
    }

    fn is_dgfx(&self) -> bool {
        self.dgfx
    }

    fn log(&mut self, event: PcodeLogEvent) {
        match event {
            PcodeLogEvent::MissingStatusCase(status) => {
                axlog::warn!("intel-pcode: unhandled mailbox status {status:#x}");
            }
            PcodeLogEvent::ReadFailure { mailbox, error } => {
                axlog::warn!("intel-pcode: read mailbox {mailbox:#x} failed: {error}");
            }
            PcodeLogEvent::WriteFailure {
                mailbox,
                value,
                error,
            } => {
                axlog::warn!(
                    "intel-pcode: write {value:#x} to mailbox {mailbox:#x} failed: {error}"
                );
            }
            PcodeLogEvent::RetryWithPreemptionDisabled => {
                axlog::debug!("intel-pcode: retrying request with preemption disabled");
            }
            PcodeLogEvent::TimeoutBaseExceedsExpected(timeout) => {
                axlog::warn!("intel-pcode: request timeout base {timeout}ms exceeds expected");
            }
            PcodeLogEvent::WaitingForHardwareInitialization => {
                axlog::info!("intel-pcode: waiting for hardware initialization");
            }
        }
    }

    fn warn_on_once_timeout_base(&mut self, timeout_base_ms: i32) {
        if !WARNED_LONG_TIMEOUT.swap(true, Ordering::AcqRel) {
            self.log(PcodeLogEvent::TimeoutBaseExceedsExpected(
                timeout_base_ms as u32,
            ));
        }
    }

    fn preempt_disable(&mut self) {
        if self.preempt_guard.is_some() {
            self.failure = Some(PcodeError::LockInvariant);
        } else {
            self.preempt_guard = Some(NoPreempt::new());
        }
    }

    fn preempt_enable(&mut self) {
        if self.preempt_guard.take().is_none() {
            self.failure = Some(PcodeError::LockInvariant);
        }
    }

    fn wait_for<F>(&mut self, mut condition: F, timeout_us: i32, _min_us: i32, _max_us: i32) -> i32
    where
        F: FnMut(&mut Self) -> bool,
    {
        let start = self.timer.now_micros();
        let budget = u64::try_from(timeout_us.max(0)).unwrap_or(0);
        loop {
            if condition(self) {
                return 0;
            }
            if self.timer.now_micros().saturating_sub(start) >= budget {
                return -intel_pcode_full::ETIMEDOUT;
            }
            self.pause();
        }
    }

    fn wait_for_atomic<F>(&mut self, mut condition: F, timeout_ms: i32) -> i32
    where
        F: FnMut(&mut Self) -> bool,
    {
        let start = self.timer.now_micros();
        let budget = u64::try_from(timeout_ms.max(0))
            .unwrap_or(0)
            .saturating_mul(1_000);
        loop {
            if condition(self) {
                return 0;
            }
            if self.timer.now_micros().saturating_sub(start) >= budget {
                return -intel_pcode_full::ETIMEDOUT;
            }
            self.pause();
        }
    }

    fn runtime_pm_get(&mut self) {
        // No runtime-PM owner exists in this kernel. The corresponding source
        // `*_p` call is refused before the next MMIO access.
        self.failure = Some(PcodeError::RuntimePmUnavailable);
    }

    fn runtime_pm_put(&mut self) {}
}

/// Prepare for an ADL-N CDCLK change. PCode status is checked before any
/// CDCLK register write; a missing mailbox/register never becomes success.
pub(crate) fn prepare_cdclk_change<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
) -> Result<(), PcodeError> {
    let mut io = RegisterPcode::new(regs, timer, 12, false);
    let status = intel_pcode_full::skl_pcode_request(
        &mut io,
        intel_pcode_full::SKL_PCODE_CDCLK_CONTROL,
        intel_pcode_full::SKL_CDCLK_PREPARE_FOR_CHANGE,
        intel_pcode_full::SKL_CDCLK_READY_FOR_CHANGE,
        intel_pcode_full::SKL_CDCLK_READY_FOR_CHANGE,
        3,
    );
    io.result(status)
}

/// Report the clock that was programmed to PCode after the source sequence.
pub(crate) fn commit_cdclk_voltage<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
    cdclk_khz: u32,
) -> Result<(), PcodeError> {
    let voltage_level = if cdclk_khz <= 312_000 {
        0
    } else if cdclk_khz <= 556_800 {
        1
    } else {
        2
    };
    let mut io = RegisterPcode::new(regs, timer, 12, false);
    let status = intel_pcode_full::snb_pcode_write_timeout(
        &mut io,
        intel_pcode_full::SKL_PCODE_CDCLK_CONTROL,
        voltage_level,
        1,
    );
    io.result(status)
}

/// Read one source-defined GEN9 display memory-latency dword. The mailbox
/// command is constant; the 0/1 selector is the DATA input value, as required
/// by `skl_read_wm_latency()` before `intel_pcode_read()`.
pub(crate) fn read_wm_latency<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
    index: u32,
) -> Result<u32, PcodeError> {
    if index > 1 {
        return Err(PcodeError::MailboxStatus(-intel_pcode_full::EINVAL));
    }
    let mailbox = intel_pcode_full::GEN9_PCODE_READ_MEM_LATENCY;
    let mut value = index;
    let mut io = RegisterPcode::new(regs, timer, 12, false);
    let status = intel_pcode_full::snb_pcode_read(&mut io, mailbox, &mut value, None);
    io.result(status)?;
    Ok(value)
}

/// Read the display-12/13 PCode SAGV block time. i915 treats an unavailable
/// value as zero for watermark calculations and logs the failure at its caller.
pub(crate) fn read_sagv_block_time_us<R: Registers, T: PollTimer>(
    regs: &R,
    timer: &T,
) -> Result<u32, PcodeError> {
    let mut value = 0u32;
    let mut io = RegisterPcode::new(regs, timer, 12, false);
    let status = intel_pcode_full::snb_pcode_read(
        &mut io,
        intel_pcode_full::GEN12_PCODE_READ_SAGV_BLOCK_TIME_US,
        &mut value,
        None,
    );
    io.result(status)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use spin::Mutex;

    use super::*;
    use crate::drm::intel::regs::Register;

    #[derive(Default)]
    struct State {
        mailbox: u32,
        data: u32,
        requests: Vec<(u32, u32)>,
    }

    #[derive(Default)]
    struct PcodeModel(Mutex<State>);

    impl Registers for PcodeModel {
        fn read(&self, register: Register) -> Option<u32> {
            let state = self.0.lock();
            Some(match register.offset() {
                offset if offset == regs::pcode::GEN6_PCODE_MAILBOX.offset() => state.mailbox,
                offset if offset == regs::pcode::GEN6_PCODE_DATA.offset() => state.data,
                offset if offset == regs::pcode::GEN6_PCODE_DATA1.offset() => 0,
                _ => return None,
            })
        }

        fn read64(&self, _register: Register) -> Option<u64> {
            None
        }

        fn write(&self, register: Register, value: u32) -> bool {
            let mut state = self.0.lock();
            match register.offset() {
                offset if offset == regs::pcode::GEN6_PCODE_DATA.offset() => state.data = value,
                offset if offset == regs::pcode::GEN6_PCODE_DATA1.offset() => {}
                offset if offset == regs::pcode::GEN6_PCODE_MAILBOX.offset() => {
                    if value & intel_pcode_full::GEN6_PCODE_READY != 0 {
                        let selector = state.data;
                        state
                            .requests
                            .push((value & !intel_pcode_full::GEN6_PCODE_READY, selector));
                        state.data = match selector {
                            0 => 0x4433_2211,
                            1 => 0x8877_6655,
                            _ => return false,
                        };
                        state.mailbox = 0;
                    } else {
                        state.mailbox = value;
                    }
                }
                _ => return false,
            }
            true
        }
    }

    struct NoTime;
    impl PollTimer for NoTime {
        fn now_micros(&self) -> u64 {
            0
        }
        fn pause(&self) {}
    }

    #[test]
    fn watermark_latency_index_is_sent_as_mailbox_data() {
        let _context = crate::test_support::scheduler_test_context();
        let regs = PcodeModel::default();
        assert_eq!(read_wm_latency(&regs, &NoTime, 0), Ok(0x4433_2211));
        assert_eq!(read_wm_latency(&regs, &NoTime, 1), Ok(0x8877_6655));
        assert_eq!(
            regs.0.lock().requests,
            [
                (intel_pcode_full::GEN9_PCODE_READ_MEM_LATENCY, 0),
                (intel_pcode_full::GEN9_PCODE_READ_MEM_LATENCY, 1),
            ]
        );
    }
}
