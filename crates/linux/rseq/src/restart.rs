use crate::{RseqArea, RseqDescriptor, RseqError, RseqEventMask};

/// Pure result of handling one scheduler/signal observation for an rseq area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartDecision {
    /// No restart action is needed: either no descriptor was active or an
    /// in-range descriptor had no pending restart event.
    NoActive,
    /// A descriptor was active but the saved instruction pointer was outside
    /// its critical section; clear the area pointer only.
    ClearOnly,
    /// A descriptor was active and the saved instruction pointer was in its
    /// critical section; clear the pointer and branch to `abort_ip`.
    Abort,
}

/// Classifies one restart observation after the adapter has copied the
/// descriptor and, when an event is pending, read the signature word
/// immediately before `abort_ip`.
///
/// `descriptor == None` is the explicit `rseq_cs == 0` path when the area is
/// inactive.  When the area contains a non-zero descriptor address, `None`
/// models an adapter usercopy failure and maps to `EFAULT`.
///
/// A return with no pending scheduler/signal/migration event is a publication
/// point only: it must update CPU fields, but must not inspect the active
/// descriptor, read its signature, clear `rseq_cs`, or alter the saved IP.
/// Once a decision succeeds, [`crate::ThreadRseq::begin_resume`] consumes the
/// event snapshot even for `NoActive` and `ClearOnly`; events published after
/// that snapshot remain pending for the next gate.
pub fn decide_restart(
    area: RseqArea,
    descriptor: Option<RseqDescriptor>,
    instruction_pointer: u64,
    pending_events: RseqEventMask,
    registration_signature: u32,
    abort_signature: u32,
) -> Result<RestartDecision, RseqError> {
    if area.rseq_cs == 0 {
        return Ok(RestartDecision::NoActive);
    }

    // Linux's restart/clear work is event-driven.  In particular, userspace
    // may publish a descriptor while returning from an ordinary syscall or
    // timer tick; that return must not validate its signature or clear the
    // pointer merely because the saved IP happens to be outside the window.
    if pending_events.is_empty() {
        return Ok(RestartDecision::NoActive);
    }

    let descriptor = descriptor.ok_or(RseqError::DescriptorReadFault)?;
    if area.rseq_cs != descriptor.address() {
        return Err(RseqError::ActiveDescriptorMismatch);
    }

    // With a pending event, Linux validates the signature word before looking
    // at the saved IP or descriptor flags.  Deriving the address can itself
    // take the adapter's EFAULT path (for example when abort_ip is smaller
    // than sizeof(u32)).  The no-event path returned above deliberately does
    // not perform this user-memory access.
    let _signature_address = descriptor.signature_address()?;
    if abort_signature != registration_signature {
        return Err(RseqError::RestartSignatureMismatch);
    }

    // Structural descriptor/range validation happened when the proof-bearing
    // object was created.  This interval check intentionally precedes flags.
    if !descriptor.contains(instruction_pointer)? {
        return Ok(RestartDecision::ClearOnly);
    }

    descriptor.validate_restart_flags(area.flags)?;
    Ok(RestartDecision::Abort)
}

/// Explicit name for the final IRQ-disabled restart gate.
pub fn restart_gate(
    area: RseqArea,
    descriptor: Option<RseqDescriptor>,
    instruction_pointer: u64,
    pending_events: RseqEventMask,
    registration_signature: u32,
    abort_signature: u32,
) -> Result<RestartDecision, RseqError> {
    decide_restart(
        area,
        descriptor,
        instruction_pointer,
        pending_events,
        registration_signature,
        abort_signature,
    )
}

impl RseqArea {
    /// Operation-neutral method form of [`decide_restart`].
    pub fn restart_decision(
        self,
        descriptor: Option<RseqDescriptor>,
        instruction_pointer: u64,
        pending_events: RseqEventMask,
        registration_signature: u32,
        abort_signature: u32,
    ) -> Result<RestartDecision, RseqError> {
        decide_restart(
            self,
            descriptor,
            instruction_pointer,
            pending_events,
            registration_signature,
            abort_signature,
        )
    }
}
