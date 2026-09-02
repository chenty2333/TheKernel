//! Optional native x86_64 seccomp execution.
//!
//! Installation remains semantic-first: verification and immutable
//! interpreter publication happen regardless of whether the bounded W^X
//! arena can host a translation. This module only supplies an executor when
//! translation, publication, and its small owner allocation all succeed.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use axcbpf::{Instruction, NativeWordInput, Program};
use axerrno::{AxError, LinuxError};
use thekernel_linux_seccomp::{SeccompExecutor, VerifiedProgram};

/// Executor choice used by the two classic-BPF adapters.
///
/// The value is read exactly once while a program is admitted. The selected
/// executor is then retained by the immutable program owner and is never
/// consulted from a capture or syscall-entry hot path. Production boots keep
/// the default `Auto` value; the setter is test-only below.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutorPolicy {
    Auto        = 0,
    Interpreter = 1,
    Jit         = 2,
}

// Keep both admission policies in one atomic word. A control write that
// updates both domains therefore publishes one complete configuration; an
// admission reads only the byte belonging to its adapter.
static EXECUTOR_POLICIES: AtomicU16 = AtomicU16::new(0);

const SECCOMP_POLICY_SHIFT: u32 = 0;
const PACKET_POLICY_SHIFT: u32 = 8;
const POLICY_MASK: u16 = 0xff;

static PUBLISHED: AtomicU64 = AtomicU64::new(0);
// A publication reserves one count before the immutable filter pointer is
// made visible.  Readers use this only when taking a diagnostic snapshot;
// the execution path never touches it.
static PUBLISH_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static NATIVE_EXECUTED: AtomicU64 = AtomicU64::new(0);
static INTERPRETER_EXECUTED: AtomicU64 = AtomicU64::new(0);
static FALLBACK_POLICY_INTERPRETER: AtomicU64 = AtomicU64::new(0);
static FALLBACK_TRANSLATION: AtomicU64 = AtomicU64::new(0);
static FALLBACK_PUBLICATION: AtomicU64 = AtomicU64::new(0);
static FALLBACK_OWNER: AtomicU64 = AtomicU64::new(0);
static FALLBACK_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);
static JIT_REJECTED: AtomicU64 = AtomicU64::new(0);

/// Why an automatically selected native executor was not retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FallbackReason {
    PolicyInterpreter,
    Translation,
    Publication,
    Owner,
    Unavailable,
}

/// Explicit rejection of a force-JIT admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JitError {
    /// The Linux profile passed an instruction stream rejected by AX's
    /// generic cBPF verifier. This is malformed userspace input, not a JIT
    /// availability failure.
    InvalidProgram,
    Translation,
    Publication(AxError),
    Quarantined(AxError),
    Retained(AxError),
    Owner,
    Unavailable(AxError),
}

impl JitError {
    pub(crate) fn into_ax_error(self) -> AxError {
        match self {
            Self::InvalidProgram => AxError::InvalidInput,
            Self::Translation => LinuxError::EOPNOTSUPP.into(),
            Self::Publication(error) => error,
            Self::Quarantined(_) | Self::Retained(_) => LinuxError::EOPNOTSUPP.into(),
            Self::Owner => AxError::NoMemory,
            Self::Unavailable(error) => error,
        }
    }
}

/// Bounded seccomp cBPF counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Counters {
    pub published: u64,
    pub native_executed: u64,
    pub interpreter_executed: u64,
    pub fallback_policy_interpreter: u64,
    pub fallback_translation: u64,
    pub fallback_publication: u64,
    pub fallback_owner: u64,
    pub fallback_unavailable: u64,
    pub jit_rejected: u64,
}

pub(crate) fn counters() -> Counters {
    loop {
        // A reservation is the only counter which can be rolled back.  Do
        // not expose its transient value while the publication owner is
        // between admission and pointer publication.
        if PUBLISH_IN_FLIGHT.load(Ordering::Acquire) != 0 {
            core::hint::spin_loop();
            continue;
        }
        let first = load_counters();
        if PUBLISH_IN_FLIGHT.load(Ordering::Acquire) != 0 {
            continue;
        }
        let second = load_counters();
        if PUBLISH_IN_FLIGHT.load(Ordering::Acquire) == 0 && second.is_monotonic_from(first) {
            return second;
        }
    }
}

fn load_counters() -> Counters {
    Counters {
        published: PUBLISHED.load(Ordering::Relaxed),
        native_executed: NATIVE_EXECUTED.load(Ordering::Relaxed),
        interpreter_executed: INTERPRETER_EXECUTED.load(Ordering::Relaxed),
        fallback_policy_interpreter: FALLBACK_POLICY_INTERPRETER.load(Ordering::Relaxed),
        fallback_translation: FALLBACK_TRANSLATION.load(Ordering::Relaxed),
        fallback_publication: FALLBACK_PUBLICATION.load(Ordering::Relaxed),
        fallback_owner: FALLBACK_OWNER.load(Ordering::Relaxed),
        fallback_unavailable: FALLBACK_UNAVAILABLE.load(Ordering::Relaxed),
        jit_rejected: JIT_REJECTED.load(Ordering::Relaxed),
    }
}

impl Counters {
    fn is_monotonic_from(self, previous: Self) -> bool {
        self.published >= previous.published
            && self.native_executed >= previous.native_executed
            && self.interpreter_executed >= previous.interpreter_executed
            && self.fallback_policy_interpreter >= previous.fallback_policy_interpreter
            && self.fallback_translation >= previous.fallback_translation
            && self.fallback_publication >= previous.fallback_publication
            && self.fallback_owner >= previous.fallback_owner
            && self.fallback_unavailable >= previous.fallback_unavailable
            && self.jit_rejected >= previous.jit_rejected
    }
}

fn increment(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

fn increment_by(counter: &AtomicU64, amount: usize) {
    let amount = u64::try_from(amount).unwrap_or(u64::MAX);
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(amount))
    });
}

impl ExecutorPolicy {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Interpreter => "interpreter",
            Self::Jit => "jit",
        }
    }
}

const fn policy_bits(policy: ExecutorPolicy) -> u16 {
    policy as u16
}

fn decode_policy(raw: u16, shift: u32) -> ExecutorPolicy {
    match (raw >> shift) & POLICY_MASK {
        value if value == policy_bits(ExecutorPolicy::Interpreter) => ExecutorPolicy::Interpreter,
        value if value == policy_bits(ExecutorPolicy::Jit) => ExecutorPolicy::Jit,
        _ => ExecutorPolicy::Auto,
    }
}

/// Returns both policies from one coherent admission-policy snapshot.
pub(crate) fn executor_policies() -> (ExecutorPolicy, ExecutorPolicy) {
    let raw = EXECUTOR_POLICIES.load(Ordering::Acquire);
    (
        decode_policy(raw, SECCOMP_POLICY_SHIFT),
        decode_policy(raw, PACKET_POLICY_SHIFT),
    )
}

/// Returns the seccomp policy captured by the next admission operation.
pub(crate) fn executor_policy() -> ExecutorPolicy {
    executor_policies().0
}

/// Returns the packet policy captured by the next admission operation.
pub(crate) fn packet_executor_policy() -> ExecutorPolicy {
    executor_policies().1
}

fn update_executor_policies(
    seccomp: Option<ExecutorPolicy>,
    packet: Option<ExecutorPolicy>,
) -> (ExecutorPolicy, ExecutorPolicy) {
    let mut current = EXECUTOR_POLICIES.load(Ordering::Acquire);
    loop {
        let old = (
            decode_policy(current, SECCOMP_POLICY_SHIFT),
            decode_policy(current, PACKET_POLICY_SHIFT),
        );
        let next = (seccomp.unwrap_or(old.0) as u16)
            | ((packet.unwrap_or(old.1) as u16) << PACKET_POLICY_SHIFT);
        match EXECUTOR_POLICIES.compare_exchange_weak(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return old,
            Err(observed) => current = observed,
        }
    }
}

#[cfg(feature = "test-io-control")]
pub(crate) fn set_executor_policies_for_control(
    seccomp: Option<ExecutorPolicy>,
    packet: Option<ExecutorPolicy>,
) {
    let _ = update_executor_policies(seccomp, packet);
}

/// Reserves a published-program counter before the immutable program pointer
/// is made visible. Dropping the reservation before `commit` rolls the count
/// back exactly, so failed admission is never exposed in a stable snapshot.
pub(crate) fn try_reserve_published() -> Option<PublicationReservation> {
    PUBLISH_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    let result = PUBLISHED.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
    });
    if result.is_ok() {
        Some(PublicationReservation { committed: false })
    } else {
        PUBLISH_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        None
    }
}

pub(crate) struct PublicationReservation {
    committed: bool,
}

impl PublicationReservation {
    /// Completes the reservation after the immutable pointer is visible.
    pub(crate) fn commit(mut self) {
        self.committed = true;
        PUBLISH_IN_FLIGHT.fetch_sub(1, Ordering::Release);
    }
}

impl Drop for PublicationReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = PUBLISHED.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_sub(1)
        });
        PUBLISH_IN_FLIGHT.fetch_sub(1, Ordering::Release);
    }
}

/// Records one native seccomp execution.
pub(crate) fn record_native_executed() {
    increment(&NATIVE_EXECUTED);
}

/// Records one interpreter seccomp execution.
pub(crate) fn record_interpreter_executed() {
    increment(&INTERPRETER_EXECUTED);
}

pub(crate) fn record_interpreter_executed_many(amount: usize) {
    increment_by(&INTERPRETER_EXECUTED, amount);
}

fn record_fallback(reason: FallbackReason) {
    match reason {
        FallbackReason::PolicyInterpreter => increment(&FALLBACK_POLICY_INTERPRETER),
        FallbackReason::Translation => increment(&FALLBACK_TRANSLATION),
        FallbackReason::Publication => increment(&FALLBACK_PUBLICATION),
        FallbackReason::Owner => increment(&FALLBACK_OWNER),
        FallbackReason::Unavailable => increment(&FALLBACK_UNAVAILABLE),
    }
}

/// Attempts to build the executor selected at admission time.
///
/// `None` is returned only for `Auto` fallback or an explicit interpreter
/// policy. `Jit` never silently falls back: every translation, W^X, or owner
/// failure is returned to the syscall adapter for an explicit rejection.
pub(crate) fn try_compile(program: &VerifiedProgram) -> Result<Arc<dyn SeccompExecutor>, JitError> {
    try_compile_with_policy(program, executor_policy())
}

pub(crate) fn try_compile_with_policy(
    program: &VerifiedProgram,
    policy: ExecutorPolicy,
) -> Result<Arc<dyn SeccompExecutor>, JitError> {
    let mechanism = mechanism_program(program)?;
    if policy == ExecutorPolicy::Interpreter {
        record_fallback(FallbackReason::PolicyInterpreter);
        return Arc::try_new(MechanismExecutor {
            program: mechanism,
            code: None,
        })
        .map(|executor| executor as Arc<dyn SeccompExecutor>)
        .map_err(|_| JitError::Owner);
    }

    #[cfg(not(feature = "bpf"))]
    {
        if policy == ExecutorPolicy::Jit {
            increment(&JIT_REJECTED);
            return Err(JitError::Unavailable(LinuxError::EOPNOTSUPP.into()));
        }
        record_fallback(FallbackReason::Unavailable);
        return Arc::try_new(MechanismExecutor {
            program: mechanism,
            code: None,
        })
        .map(|executor| executor as Arc<dyn SeccompExecutor>)
        .map_err(|_| JitError::Owner);
    }

    #[cfg(feature = "bpf")]
    {
        let native = match compile_native(&mechanism) {
            Ok(native) => native,
            Err(error) => {
                if policy == ExecutorPolicy::Jit {
                    increment(&JIT_REJECTED);
                    return Err(error);
                }
                record_fallback(error.fallback_reason());
                return Arc::try_new(MechanismExecutor {
                    program: mechanism,
                    code: None,
                })
                .map(|executor| executor as Arc<dyn SeccompExecutor>)
                .map_err(|_| JitError::Owner);
            }
        };
        let owner: Arc<dyn SeccompExecutor> = match Arc::try_new(MechanismExecutor {
            program: mechanism,
            code: Some(native),
        }) {
            Ok(owner) => owner,
            Err(_) => {
                if policy == ExecutorPolicy::Jit {
                    increment(&JIT_REJECTED);
                    return Err(JitError::Owner);
                }
                record_fallback(FallbackReason::Owner);
                return Arc::try_new(MechanismExecutor {
                    program: mechanism_program(program)?,
                    code: None,
                })
                .map(|executor| executor as Arc<dyn SeccompExecutor>)
                .map_err(|_| JitError::Owner);
            }
        };
        Ok(owner)
    }
}

#[cfg(feature = "bpf")]
fn compile_native(program: &Program) -> Result<crate::jit_memory::ExecutableCode, JitError> {
    let image = match program.translate_with_profile(axcbpf::InputProfile::NativeAlignedWords) {
        Ok(image) => image,
        Err(_) => return Err(JitError::Translation),
    };

    let mut writable = match crate::jit_memory::prepare(image.bytes().len()) {
        Ok(writable) => writable,
        Err(error) => return Err(JitError::from_memory_error(error)),
    };
    if let Err(error) = writable.write(0, image.bytes()) {
        let error = writable.abort(crate::jit_memory::MemoryError::Unavailable(error));
        return Err(JitError::from_memory_error(error));
    }
    let code = match writable.publish(image.entry_offset() as usize) {
        Ok(code) => code,
        Err(error) => return Err(JitError::from_memory_error(error)),
    };
    Ok(code)
}

impl JitError {
    #[cfg(feature = "bpf")]
    fn from_memory_error(error: crate::jit_memory::MemoryError) -> Self {
        match error {
            crate::jit_memory::MemoryError::Unavailable(error) => Self::Unavailable(error),
            crate::jit_memory::MemoryError::Quarantined(error) => Self::Quarantined(error),
            crate::jit_memory::MemoryError::Retained(error) => Self::Retained(error),
        }
    }

    fn fallback_reason(self) -> FallbackReason {
        match self {
            Self::InvalidProgram => FallbackReason::Translation,
            Self::Translation => FallbackReason::Translation,
            Self::Publication(_) => FallbackReason::Publication,
            Self::Quarantined(_) | Self::Retained(_) | Self::Unavailable(_) => {
                FallbackReason::Unavailable
            }
            Self::Owner => FallbackReason::Owner,
        }
    }
}

#[cfg(feature = "bpf")]
struct MechanismExecutor {
    program: Program,
    code: Option<crate::jit_memory::ExecutableCode>,
}

#[cfg(feature = "bpf")]
impl SeccompExecutor for MechanismExecutor {
    fn execute(&self, data: &[u8]) -> u32 {
        if let Some(code) = self.code.as_ref() {
            record_native_executed();
            return code.execute(data);
        }
        record_interpreter_executed();
        NativeWordInput::new_aligned(data).map_or(0, |input| self.program.evaluate(&input))
    }
}

#[cfg(not(feature = "bpf"))]
struct MechanismExecutor {
    program: Program,
    code: Option<()>,
}

#[cfg(not(feature = "bpf"))]
impl SeccompExecutor for MechanismExecutor {
    fn execute(&self, data: &[u8]) -> u32 {
        let _ = self.code;
        record_interpreter_executed();
        NativeWordInput::new_aligned(data).map_or(0, |input| self.program.evaluate(&input))
    }
}

fn mechanism_program(program: &VerifiedProgram) -> Result<Program, JitError> {
    let mut instructions = alloc::vec::Vec::new();
    instructions
        .try_reserve_exact(program.instructions().len())
        .map_err(|_| JitError::Owner)?;
    instructions.extend(program.instructions().iter().map(|instruction| {
        Instruction::new(
            instruction.code,
            instruction.jt,
            instruction.jf,
            instruction.k,
        )
    }));
    Program::try_from_vec(instructions).map_err(|error| match error {
        axcbpf::VerifyError::NoMemory => JitError::Owner,
        _ => JitError::InvalidProgram,
    })
}

#[cfg(test)]
pub(crate) fn set_executor_policy_for_tests(policy: ExecutorPolicy) -> ExecutorPolicy {
    let old = executor_policy();
    let _ = update_executor_policies(Some(policy), None);
    old
}

#[cfg(test)]
pub(crate) fn reset_counters_for_tests() {
    for counter in [
        &PUBLISHED,
        &NATIVE_EXECUTED,
        &INTERPRETER_EXECUTED,
        &FALLBACK_POLICY_INTERPRETER,
        &FALLBACK_TRANSLATION,
        &FALLBACK_PUBLICATION,
        &FALLBACK_OWNER,
        &FALLBACK_UNAVAILABLE,
        &JIT_REJECTED,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    PUBLISH_IN_FLIGHT.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{vec, vec::Vec};

    use axcbpf::opcode;
    use thekernel_linux_seccomp::{ClassicBpfInstruction, SECCOMP_RET_ALLOW, VerifiedProgram};

    use super::*;

    fn allow_program() -> VerifiedProgram {
        VerifiedProgram::try_from_vec(vec![ClassicBpfInstruction::new(
            opcode::RET_K,
            0,
            0,
            SECCOMP_RET_ALLOW,
        )])
        .unwrap()
    }

    fn malformed_program(instructions: Vec<ClassicBpfInstruction>) -> VerifiedProgram {
        VerifiedProgram::try_from_vec(instructions).unwrap()
    }

    #[test]
    fn malformed_linux_profile_is_not_reported_as_jit_translation_failure() {
        let missing_return = malformed_program(alloc::vec![ClassicBpfInstruction::new(
            opcode::LD_IMM,
            0,
            0,
            0,
        )]);
        let out_of_range_jump = malformed_program(alloc::vec![
            ClassicBpfInstruction::new(opcode::JMP_JA, 0, 0, 1),
            ClassicBpfInstruction::new(opcode::RET_K, 0, 0, SECCOMP_RET_ALLOW),
        ]);
        for program in [&missing_return, &out_of_range_jump] {
            assert!(matches!(
                try_compile_with_policy(program, ExecutorPolicy::Jit),
                Err(JitError::InvalidProgram)
            ));
        }
    }

    #[test]
    fn interpreter_policy_is_captured_at_admission() {
        let result = try_compile_with_policy(&allow_program(), ExecutorPolicy::Interpreter);
        assert!(result.is_ok());
    }

    #[cfg(feature = "test-io-control")]
    #[test]
    fn control_policy_changes_only_future_seccomp_admissions() {
        let old = executor_policies();
        set_executor_policies_for_control(Some(ExecutorPolicy::Interpreter), None);
        let old_program_executor = try_compile(&allow_program()).unwrap();
        set_executor_policies_for_control(Some(ExecutorPolicy::Jit), None);
        let new_program_executor = try_compile(&allow_program());

        assert_eq!(old_program_executor.execute(&[0; 64]), SECCOMP_RET_ALLOW);
        assert!(new_program_executor.is_err());

        set_executor_policies_for_control(Some(old.0), Some(old.1));
    }

    #[cfg(feature = "test-io-control")]
    #[test]
    fn control_policies_are_independent() {
        let old = executor_policies();
        set_executor_policies_for_control(
            Some(ExecutorPolicy::Interpreter),
            Some(ExecutorPolicy::Jit),
        );
        assert_eq!(
            executor_policies(),
            (ExecutorPolicy::Interpreter, ExecutorPolicy::Jit)
        );
        set_executor_policies_for_control(Some(old.0), Some(old.1));
    }

    #[cfg(not(feature = "bpf"))]
    #[test]
    fn force_jit_rejects_without_interpreter_fallback() {
        let result = try_compile_with_policy(&allow_program(), ExecutorPolicy::Jit);
        assert!(matches!(result, Err(JitError::Unavailable(_))));
    }

    #[cfg(feature = "bpf")]
    #[test]
    fn force_jit_never_returns_an_interpreter_fallback() {
        let before = counters();
        let result = try_compile_with_policy(&allow_program(), ExecutorPolicy::Jit);
        match result {
            Ok(executor) => {
                assert_eq!(executor.execute(&[0; 64]), SECCOMP_RET_ALLOW);
                let after = counters();
                assert_eq!(after.native_executed, before.native_executed + 1);
                assert_eq!(after.interpreter_executed, before.interpreter_executed);
            }
            Err(_) => {
                let after = counters();
                assert_eq!(after.jit_rejected, before.jit_rejected + 1);
                assert_eq!(after.interpreter_executed, before.interpreter_executed);
            }
        }
    }

    #[test]
    fn publication_does_not_count_as_execution() {
        let before = counters();
        try_reserve_published().unwrap().commit();
        let after = counters();
        assert_eq!(after.published, before.published.saturating_add(1));
        assert_eq!(after.native_executed, before.native_executed);
        assert_eq!(after.interpreter_executed, before.interpreter_executed);
    }

    #[test]
    fn failed_publication_reservation_is_not_observable() {
        let before = counters();
        let reservation = try_reserve_published().unwrap();
        assert!(load_counters().published > before.published);
        drop(reservation);
        assert_eq!(counters().published, before.published);
    }

    #[test]
    fn publication_is_reserved_before_execution_can_be_counted() {
        let before = counters();
        let reservation = try_reserve_published().unwrap();
        assert!(load_counters().published >= before.published.saturating_add(1));
        record_interpreter_executed();
        reservation.commit();
        let after = counters();
        assert!(after.published >= before.published.saturating_add(1));
        assert!(after.interpreter_executed >= before.interpreter_executed.saturating_add(1));
    }

    #[test]
    fn saturating_counter_increment_is_concurrent() {
        use std::{sync::Arc, thread};

        let counter = Arc::new(AtomicU64::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let counter = counter.clone();
            workers.push(thread::spawn(move || {
                for _ in 0..1_000 {
                    increment(&counter);
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::Relaxed), 4_000);
    }
}
