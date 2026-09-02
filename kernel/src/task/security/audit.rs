//! Generic audit event service.
//!
//! Policy code owns its decisions and accounting; this module owns the one
//! ordered transport hand-off. Audit records are delivered through
//! `NETLINK_AUDIT`, never reduced to printk-only diagnostics.

use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub(crate) struct AuditLandlockDenied {
    pub(crate) domain_id: u64,
    pub(crate) access: u64,
    pub(crate) blocker: &'static str,
    pub(crate) on_exec: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct AuditSeccompDecision {
    pub(crate) pid: u32,
    pub(crate) syscall: i32,
    pub(crate) architecture: u32,
    pub(crate) instruction_pointer: u64,
    pub(crate) action: u32,
}

static AUDIT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn emit_landlock_denial(event: AuditLandlockDenied) {
    let sequence = AUDIT_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    // Audit is observational.  A full listener queue must never turn a
    // Landlock denial into a different security decision.
    crate::file::netlink::emit_landlock_audit(sequence, event);
}

/// Emits one kernel-originated `AUDIT_SECCOMP` record. Transport congestion is
/// observational and never changes the already-selected seccomp decision.
pub(crate) fn emit_seccomp_decision(event: AuditSeccompDecision) {
    let sequence = AUDIT_SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    crate::file::netlink::emit_seccomp_audit(sequence, event);
}
