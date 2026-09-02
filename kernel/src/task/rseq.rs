//! Thread-local Linux restartable-sequence adapter state.

use alloc::sync::Arc;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::{
    paging::MappingFlags,
    percpu,
    uspace::{UserContext, UserReturnHookAction},
};
use axsync::{Mutex, spin::SpinNoIrq};
use kernel_guard::NoPreemptIrqSave;
use thekernel_linux_rseq::{
    ForkMode, ForkPlan, RestartDecision, ResumePlan, RseqArea, RseqCriticalSection, RseqDescriptor,
    RseqError, RseqEventMask, RseqRegistration, ThreadRseq, UserAddressLimit, decode_area,
    decode_critical_section,
};

use super::Thread;
use crate::mm::{
    UserNofaultError, fault_user_range_task, read_user_nofault_task, try_user_nofault_transaction,
};

/// Size of the kernel-maintained rseq feature prefix advertised to a new
/// image. The registration ABI still requires the full 32-byte area, but this
/// profile publishes the complete base area.  `mm_cid` is currently zero for
/// the single-mm-cid implementation, but it remains a kernel-owned field and
/// must be refreshed with the CPU/node tuple on every return-to-user edge.
pub(crate) const AT_RSEQ_FEATURE_SIZE: usize = 32;
/// Alignment of the Linux v6.6 rseq ABI area advertised to a new user image.
pub(crate) const AT_RSEQ_ALIGN: usize = thekernel_linux_rseq::RSEQ_AREA_ALIGN;

impl Thread {
    /// Runs one operation against this thread's serialized rseq state.
    pub(crate) fn with_rseq_state<R>(&self, f: impl FnOnce(&mut ThreadRseq) -> R) -> R {
        f(&mut self.rseq.lock())
    }

    /// Reserves the child rseq snapshot before clone starts its fallible
    /// construction.  Dropping the returned guard cancels the reservation;
    /// commit is performed only once the child is ready for publication.
    pub(crate) fn prepare_rseq_fork(&self, clone_vm: bool) -> AxResult<RseqForkReservation<'_>> {
        let mode = if clone_vm {
            ForkMode::CloneVm
        } else {
            ForkMode::PrivateVm
        };
        let plan = self
            .with_rseq_state(|state| state.prepare_fork(mode))
            .map_err(map_rseq_error)?;
        Ok(RseqForkReservation {
            thread: self,
            plan: Some(plan),
        })
    }

    /// Reserves the exec lifecycle transition.  The reservation is cancelled
    /// automatically if any pre-commit image preparation fails.
    pub(crate) fn prepare_rseq_exec(&self) -> AxResult<RseqExecReservation<'_>> {
        let plan = self
            .with_rseq_state(|state| state.prepare_exec())
            .map_err(map_rseq_error)?;
        Ok(RseqExecReservation {
            thread: self,
            plan: Some(plan),
        })
    }

    /// Installs a committed child snapshot without touching user memory.
    pub(crate) fn install_rseq_state(&self, state: ThreadRseq) {
        *self.rseq.lock() = state;
    }

    /// Drops all rseq state at task exit.  Exit deliberately does not access
    /// the registered user area; the user mapping may already be gone.
    pub(crate) fn reset_rseq_on_exit(&self) {
        *self.rseq.lock() = ThreadRseq::new();
    }

    /// Records a scheduler/signal/migration observation for the next rseq
    /// return gate.
    ///
    /// This only publishes the bounded event flag. It does not clear user
    /// memory, inspect a critical-section descriptor, or alter a saved return
    /// context; those final-return semantics are intentionally not claimed by
    /// this phase of the kernel integration.
    pub(crate) fn notify_rseq(&self, events: RseqEventMask) -> Result<(), RseqError> {
        self.with_rseq_state(|state| {
            // Linux only tracks restart observations for an active
            // registration. Do not carry scheduler/signal events across an
            // unregister/register pair.
            if state.registration().is_some() {
                state.notify(events)
            } else {
                Ok(())
            }
        })
    }

    /// Resolves a recoverable nofault result in task context before the next
    /// IRQ-disabled return attempt.
    ///
    /// The area is populated writable (which also completes a post-fork COW
    /// fault), then the copied area is used to discover and populate the
    /// descriptor and its signature word. A failure after this recovery pass
    /// is terminal for the current return; converting an unexpected second
    /// nofault Retry to `BadAddress` prevents an unbounded reschedule loop.
    pub(crate) fn prepare_rseq_retry(
        &self,
        aspace: &Arc<Mutex<crate::mm::AddrSpace>>,
    ) -> Result<(), UserNofaultError> {
        let Some(registration) = self.with_rseq_state(|state| state.registration()) else {
            return Ok(());
        };
        let area_address = usize::try_from(registration.area_address())
            .map_err(|_| UserNofaultError::BadAddress)?;

        // The return gate always publishes CPU fields, so the area must be
        // writable even when no critical section is active. `populate_area`
        // performs the actual missing-page allocation and COW remap here.
        fault_user_range_task(
            aspace,
            area_address,
            thekernel_linux_rseq::RSEQ_AREA_SIZE,
            MappingFlags::WRITE,
        )?;

        let mut area_bytes = [0u8; thekernel_linux_rseq::RSEQ_AREA_SIZE];
        read_user_nofault_task(area_address, aspace, &mut area_bytes).map_err(
            |error| match error {
                UserNofaultError::Retry => UserNofaultError::BadAddress,
                error => error,
            },
        )?;
        let area = decode_rseq_area(&area_bytes);
        // Descriptor and signature pages are only part of the recovery span
        // when a scheduler/signal/migration observation is pending. An
        // ordinary return may see a freshly published rseq_cs; resolving that
        // descriptor here would reintroduce the same no-event fault that the
        // final gate deliberately avoids.
        if area.rseq_cs == 0 || self.with_rseq_state(|state| state.pending_events().is_empty()) {
            return Ok(());
        }

        let user_limit = user_address_limit();
        if !user_limit.contains(area.rseq_cs) {
            return Err(UserNofaultError::BadAddress);
        }
        let descriptor_address =
            usize::try_from(area.rseq_cs).map_err(|_| UserNofaultError::BadAddress)?;
        fault_user_range_task(
            aspace,
            descriptor_address,
            thekernel_linux_rseq::RSEQ_CS_SIZE,
            MappingFlags::READ,
        )?;
        let mut descriptor_bytes = [0u8; thekernel_linux_rseq::RSEQ_CS_SIZE];
        read_user_nofault_task(descriptor_address, aspace, &mut descriptor_bytes).map_err(
            |error| match error {
                UserNofaultError::Retry => UserNofaultError::BadAddress,
                error => error,
            },
        )?;
        let critical_section = decode_rseq_critical_section(&descriptor_bytes);
        let descriptor = RseqDescriptor::from_user(area.rseq_cs, critical_section, user_limit)
            .map_err(|_| UserNofaultError::BadAddress)?;
        let signature_address = descriptor
            .signature_address()
            .map_err(|_| UserNofaultError::BadAddress)?;
        let signature_address =
            usize::try_from(signature_address).map_err(|_| UserNofaultError::BadAddress)?;
        fault_user_range_task(aspace, signature_address, 4, MappingFlags::READ)
    }

    /// Final IRQ-disabled rseq return gate used immediately before entering
    /// user mode.  The address-space handle is explicit so this path never
    /// resolves an implicit `current` task and never takes a blocking lock.
    pub(crate) fn rseq_return_gate(
        &self,
        uctx: &mut UserContext,
        aspace: &Arc<Mutex<crate::mm::AddrSpace>>,
    ) -> UserReturnHookAction {
        let Some(registration) = self.with_rseq_state(|state| state.registration()) else {
            return UserReturnHookAction::EnterUser;
        };

        let area_address = match usize::try_from(registration.area_address()) {
            Ok(address) => address,
            Err(_) => return UserReturnHookAction::Fault,
        };
        let result = try_user_nofault_transaction(aspace, |transaction| {
            // Keep every read and destination preflight under the same
            // address-space guard. No mapping can change between the clear
            // and publication writes once this closure starts committing.
            let mut area_bytes = [0u8; thekernel_linux_rseq::RSEQ_AREA_SIZE];
            transaction.read(area_address, &mut area_bytes)?;
            let area = decode_rseq_area(&area_bytes);

            // Descriptor validation and signature access are driven only by a
            // pending PREEMPT/SIGNAL/MIGRATE observation. Ordinary user
            // returns still publish CPU fields, but must not clear an active
            // rseq_cs or fault on a descriptor which userspace has just
            // published outside a critical window.
            let has_pending_event =
                self.with_rseq_state(|state| !state.pending_events().is_empty());
            let (descriptor, abort_signature, abort_ip) = if !has_pending_event || area.rseq_cs == 0
            {
                (None, 0, None)
            } else {
                let user_limit = user_address_limit();
                // Linux validates the descriptor pointer before attempting
                // its 32-byte copy. Keep an out-of-range pointer in the
                // policy/EINVAL path instead of turning it into a copy fault.
                if !user_limit.contains(area.rseq_cs) {
                    return Ok(map_rseq_gate_action(RseqError::AddressOutOfRange));
                }
                let descriptor_address = match usize::try_from(area.rseq_cs) {
                    Ok(address) => address,
                    Err(_) => return Ok(UserReturnHookAction::Fault),
                };
                let mut descriptor_bytes = [0u8; thekernel_linux_rseq::RSEQ_CS_SIZE];
                transaction.read(descriptor_address, &mut descriptor_bytes)?;
                let critical_section = decode_rseq_critical_section(&descriptor_bytes);
                let descriptor =
                    match RseqDescriptor::from_user(area.rseq_cs, critical_section, user_limit) {
                        Ok(descriptor) => descriptor,
                        Err(error) => return Ok(map_rseq_gate_action(error)),
                    };
                let signature_address = match descriptor.signature_address() {
                    Ok(address) => match usize::try_from(address) {
                        Ok(address) => address,
                        Err(_) => return Ok(UserReturnHookAction::Fault),
                    },
                    Err(error) => return Ok(map_rseq_gate_action(error)),
                };
                let mut signature_bytes = [0u8; 4];
                transaction.read(signature_address, &mut signature_bytes)?;
                (
                    Some(descriptor),
                    u32::from_ne_bytes(signature_bytes),
                    Some(critical_section.abort_ip),
                )
            };

            // `begin_resume` performs the policy-defined signature -> saved-IP
            // / flags -> pending-events ordering. It also reserves pending
            // events before any user memory or saved-IP side effect.
            let plan = match self.with_rseq_state(|state| {
                state.begin_resume(
                    area,
                    descriptor,
                    uctx.ip() as u64,
                    registration.signature(),
                    abort_signature,
                )
            }) {
                Ok(plan) => plan,
                Err(error) => return Ok(map_rseq_gate_action(error)),
            };
            let resume = RseqResumeReservation::new(&self.rseq, plan);

            let clear_pointer = matches!(
                resume.decision(),
                RestartDecision::ClearOnly | RestartDecision::Abort
            );
            let clear_address = if clear_pointer {
                match area_address.checked_add(8) {
                    Some(address) => Some(address),
                    None => return Ok(UserReturnHookAction::Fault),
                }
            } else {
                None
            };

            // Build and validate all writes before the first byte changes.
            // This is the transaction's no-partial-write boundary.
            let cpu = match u32::try_from(percpu::this_cpu_id()) {
                Ok(cpu) => cpu,
                Err(_) => return Ok(UserReturnHookAction::Fault),
            };
            let cpu_id_start_address = area_address;
            let cpu_id_address = match area_address.checked_add(4) {
                Some(address) => address,
                None => return Ok(UserReturnHookAction::Fault),
            };
            let node_id_address = match area_address.checked_add(20) {
                Some(address) => address,
                None => return Ok(UserReturnHookAction::Fault),
            };
            let mm_cid_address = match area_address.checked_add(24) {
                Some(address) => address,
                None => return Ok(UserReturnHookAction::Fault),
            };
            let cpu_bytes = cpu.to_ne_bytes();
            let node_bytes = 0u32.to_ne_bytes();
            let mm_cid_bytes = 0u32.to_ne_bytes();
            let clear = [0u8; 8];
            let abort_ip = if resume.decision() == RestartDecision::Abort {
                match abort_ip {
                    Some(abort_ip) => Some(abort_ip),
                    None => return Ok(UserReturnHookAction::Fault),
                }
            } else {
                None
            };

            // Every destination is checked while the same address-space guard
            // is held. Only kernel-owned scalar fields are ever written; in
            // particular, flags and an active user rseq_cs are never copied
            // back from the stale area snapshot.
            if let Some(clear_address) = clear_address {
                transaction.preflight_write(clear_address, &clear)?;
            }
            transaction.preflight_write(cpu_id_start_address, &cpu_bytes)?;
            transaction.preflight_write(cpu_id_address, &cpu_bytes)?;
            transaction.preflight_write(node_id_address, &node_bytes)?;
            transaction.preflight_write(mm_cid_address, &mm_cid_bytes)?;

            if let Some(clear_address) = clear_address {
                transaction.write(clear_address, &clear)?;
            }
            if let Some(abort_ip) = abort_ip {
                // The rseq_cs clear is visible before the saved return IP
                // changes, and both happen before CPU identity publication.
                uctx.set_ip(abort_ip as usize);
            }
            transaction.write(cpu_id_start_address, &cpu_bytes)?;
            transaction.write(cpu_id_address, &cpu_bytes)?;
            transaction.write(node_id_address, &node_bytes)?;
            transaction.write(mm_cid_address, &mm_cid_bytes)?;

            resume.commit();
            Ok(UserReturnHookAction::EnterUser)
        });
        match result {
            Ok(action) => action,
            Err(error) => map_user_nofault_action(error),
        }
    }

    /// Signal delivery must publish its observation and perform any rseq abort
    /// before the signal layer changes the saved user IP to a handler frame.
    /// The caller supplies the target context and address space explicitly.
    pub(crate) fn pre_signal_rseq_delivery(
        &self,
        uctx: &mut UserContext,
        aspace: &Arc<Mutex<crate::mm::AddrSpace>>,
    ) -> UserReturnHookAction {
        let _guard = NoPreemptIrqSave::new();
        if let Err(error) = self.notify_rseq(RseqEventMask::SIGNAL) {
            return map_rseq_gate_action(error);
        }
        self.rseq_return_gate(uctx, aspace)
    }
}

/// Owns one restart decision until its user-memory/IP side effects commit.
/// Every early return, including a clear-only or no-active write failure,
/// restores the policy reservation through `cancel_resume`.
struct RseqResumeReservation<'a> {
    state: &'a SpinNoIrq<ThreadRseq>,
    plan: Option<ResumePlan>,
}

impl<'a> RseqResumeReservation<'a> {
    fn new(state: &'a SpinNoIrq<ThreadRseq>, plan: ResumePlan) -> Self {
        Self {
            state,
            plan: Some(plan),
        }
    }

    fn decision(&self) -> RestartDecision {
        self.plan
            .as_ref()
            .expect("rseq resume plan consumed before commit")
            .decision()
    }

    fn commit(mut self) {
        let plan = self
            .plan
            .take()
            .expect("rseq resume reservation committed twice");
        self.state.lock().commit_resume(plan);
    }
}

impl Drop for RseqResumeReservation<'_> {
    fn drop(&mut self) {
        if let Some(plan) = self.plan.take() {
            self.state.lock().cancel_resume(plan);
        }
    }
}

/// A fork reservation that cancels itself if child construction fails.
pub(crate) struct RseqForkReservation<'a> {
    thread: &'a Thread,
    plan: Option<ForkPlan>,
}

impl RseqForkReservation<'_> {
    pub(crate) fn commit(mut self) -> ThreadRseq {
        let plan = self
            .plan
            .take()
            .expect("rseq fork reservation committed twice");
        self.thread.with_rseq_state(|state| state.commit_fork(plan))
    }
}

impl Drop for RseqForkReservation<'_> {
    fn drop(&mut self) {
        if let Some(plan) = self.plan.take() {
            self.thread.with_rseq_state(|state| state.cancel_fork(plan));
        }
    }
}

/// An exec reservation that leaves registration/events intact on failure.
pub(crate) struct RseqExecReservation<'a> {
    thread: &'a Thread,
    plan: Option<thekernel_linux_rseq::ExecPlan>,
}

impl RseqExecReservation<'_> {
    pub(crate) fn commit(mut self) -> Option<RseqRegistration> {
        let plan = self
            .plan
            .take()
            .expect("rseq exec reservation committed twice");
        self.thread
            .with_rseq_state(|state| state.on_exec_success(plan))
    }
}

impl Drop for RseqExecReservation<'_> {
    fn drop(&mut self) {
        if let Some(plan) = self.plan.take() {
            self.thread.with_rseq_state(|state| state.cancel_exec(plan));
        }
    }
}

fn map_rseq_error(error: RseqError) -> AxError {
    match error.errno() {
        thekernel_linux_rseq::ErrnoClass::InvalidArgument => AxError::InvalidInput,
        thekernel_linux_rseq::ErrnoClass::PermissionDenied => LinuxError::EPERM.into(),
        thekernel_linux_rseq::ErrnoClass::Busy => LinuxError::EBUSY.into(),
        thekernel_linux_rseq::ErrnoClass::Fault => LinuxError::EFAULT.into(),
        thekernel_linux_rseq::ErrnoClass::Stale => LinuxError::EAGAIN.into(),
        thekernel_linux_rseq::ErrnoClass::Overflow => LinuxError::EOVERFLOW.into(),
    }
}

fn user_address_limit() -> UserAddressLimit {
    // Linux x86_64 TASK_SIZE is an exclusive bound. The low page is excluded
    // by the mapping policy separately; descriptor/code proofs must not widen
    // the upper edge to 0x8000_0000_0000.
    UserAddressLimit::new(0x7fff_ffff_f000)
}

fn map_user_nofault_action(error: UserNofaultError) -> UserReturnHookAction {
    match error {
        UserNofaultError::Retry => UserReturnHookAction::Retry,
        UserNofaultError::BadAddress => UserReturnHookAction::Fault,
    }
}

fn map_rseq_gate_action(error: RseqError) -> UserReturnHookAction {
    match error {
        RseqError::OperationInProgress => UserReturnHookAction::Retry,
        RseqError::DescriptorReadFault | RseqError::SignatureAddressUnderflow => {
            UserReturnHookAction::Fault
        }
        _ => UserReturnHookAction::Fault,
    }
}

fn decode_rseq_area(bytes: &[u8; thekernel_linux_rseq::RSEQ_AREA_SIZE]) -> RseqArea {
    decode_area(bytes).expect("fixed rseq area has ABI-required size")
}

fn decode_rseq_critical_section(
    bytes: &[u8; thekernel_linux_rseq::RSEQ_CS_SIZE],
) -> RseqCriticalSection {
    decode_critical_section(bytes).expect("fixed rseq_cs has ABI-required size")
}

#[cfg(test)]
mod tests {
    extern crate std;

    use thekernel_linux_rseq::RseqRegistrationRequest;

    use super::*;

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn read_u64(bytes: &[u8], offset: usize) -> u64 {
        u64::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    #[test]
    fn failed_resume_reservation_restores_reserved_abort_events() {
        let state = SpinNoIrq::new(ThreadRseq::new());
        let register = state
            .lock()
            .prepare_register(RseqRegistrationRequest::new(0x1000, 32, 0x55aa))
            .unwrap();
        state.lock().commit_register(register);
        state.lock().notify(RseqEventMask::PREEMPT).unwrap();

        let area = RseqArea {
            rseq_cs: 0x2000,
            ..RseqArea::new()
        };
        let critical_section = RseqCriticalSection::new(0x1000, 0x100, 0x3004);
        let descriptor = RseqDescriptor::new(
            area.rseq_cs,
            critical_section,
            UserAddressLimit::new(0x1_0000),
        )
        .unwrap();
        let plan = state
            .lock()
            .begin_resume(area, Some(descriptor), 0x1001, 0x55aa, 0x55aa)
            .unwrap();
        assert_eq!(plan.decision(), RestartDecision::Abort);

        // Model a nofault/validation failure after begin_resume. Drop must
        // return the reserved event to the next gate rather than leaking it.
        drop(RseqResumeReservation::new(&state, plan));

        let state = state.lock();
        assert_eq!(state.pending_events(), RseqEventMask::PREEMPT);
        assert!(!state.has_pending_operation());
    }

    #[test]
    fn cpu_publication_preserves_user_owned_rseq_fields() {
        let mut area = [0u8; thekernel_linux_rseq::RSEQ_AREA_SIZE];
        area[8..16].copy_from_slice(&0x2000_u64.to_ne_bytes());
        area[16..20].copy_from_slice(&0xa5a5_a5a5_u32.to_ne_bytes());
        area[24..28].copy_from_slice(&0x5a5a_5a5a_u32.to_ne_bytes());
        let original_rseq_cs = read_u64(&area, 8);
        let original_flags = read_u32(&area, 16);

        // Model the exact scalar writes used by the gate. No 32-byte stale
        // snapshot is copied back; mm_cid is refreshed as a kernel-owned
        // publication field.
        area[0..4].copy_from_slice(&3u32.to_ne_bytes());
        area[4..8].copy_from_slice(&3u32.to_ne_bytes());
        area[20..24].copy_from_slice(&0u32.to_ne_bytes());
        area[24..28].copy_from_slice(&0u32.to_ne_bytes());
        assert_eq!(read_u64(&area, 8), original_rseq_cs);
        assert_eq!(read_u32(&area, 16), original_flags);
        assert_eq!(read_u32(&area, 24), 0);

        // A restart clear is a separate single 8-byte field write.
        area[8..16].fill(0);
        assert_eq!(read_u64(&area, 8), 0);
        assert_eq!(read_u32(&area, 16), original_flags);
        assert_eq!(read_u32(&area, 24), 0);
    }

    #[test]
    fn auxv_feature_size_includes_the_complete_base_area() {
        assert_eq!(AT_RSEQ_FEATURE_SIZE, 32);
        assert_eq!(AT_RSEQ_ALIGN, thekernel_linux_rseq::RSEQ_AREA_ALIGN);
        assert_eq!(thekernel_linux_rseq::RSEQ_ABI_SIZE, 32);
    }

    #[test]
    fn x86_task_size_is_the_exclusive_descriptor_limit() {
        let limit = user_address_limit();
        assert_eq!(limit.exclusive(), 0x7fff_ffff_f000);
        assert!(limit.contains(0x7fff_ffff_eff0));
        assert!(!limit.contains(0x7fff_ffff_f000));
    }
}
