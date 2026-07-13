//! Kernel transaction and security-hook adapter for Linux exec credentials.
//!
//! Linux-visible derivation lives in `thekernel-linux-cred`. This module keeps
//! the writer guard, hook call, and unpublished publication token together so
//! a denied or dropped proposal has no observable effect.

use alloc::sync::Arc;

use axerrno::AxResult;
pub(crate) use thekernel_linux_cred::{
    ExecAuxIdentity, ExecCredentialEffects, ExecCredentialInput, ExecDumpability, ExecFileOwner,
    ExecImageReadability, ExecMountPrivilege, ExecPtraceRevalidation, ExecTraceState,
};
use thekernel_linux_cred::{
    ExecCredentialProposal, commoncap_exec_transition, derive_exec_credential,
};

use super::{
    Dumpability, FileCapabilities, Thread, UserNamespace, cred_error,
    creds::{Cred, CredentialUpdate, PreparedCred},
};

/// Maps the policy-neutral parser error at the kernel adapter boundary.
pub(crate) fn parse_file_capabilities(value: &[u8]) -> AxResult<FileCapabilities> {
    thekernel_linux_cred::parse_file_capabilities(value).map_err(cred_error)
}

/// Maps a Linux exec policy decision into the process layer's implemented
/// dumpability state. Address-space ownership and publication remain local.
pub(crate) const fn map_exec_dumpability(value: ExecDumpability) -> Dumpability {
    match value {
        ExecDumpability::NotDumpable => Dumpability::NotDumpable,
        ExecDumpability::UserDumpable => Dumpability::UserDumpable,
        _ => Dumpability::NotDumpable,
    }
}

/// Typed authorization view over one opaque, exact-old-bound proposal.
///
/// The proposal is only borrowed. A hook cannot extract its proposed `Arc` or
/// publish it; successful authorization returns control to the writer
/// transaction, which consumes it through the exact-old check.
pub(crate) struct ExecCredentialSecurityContext<'a> {
    proposal: &'a ExecCredentialProposal<UserNamespace>,
}

impl<'a> ExecCredentialSecurityContext<'a> {
    pub(in crate::task) fn new(proposal: &'a ExecCredentialProposal<UserNamespace>) -> Self {
        Self { proposal }
    }

    pub(in crate::task) const fn proposal(&self) -> &ExecCredentialProposal<UserNamespace> {
        self.proposal
    }

    pub(in crate::task) fn old(&self) -> &Cred {
        self.proposal.old()
    }

    pub(in crate::task) fn proposed(&self) -> &Cred {
        self.proposal.proposed()
    }

    pub(in crate::task) const fn input(&self) -> ExecCredentialInput {
        self.proposal.input()
    }

    pub(in crate::task) const fn effects(&self) -> ExecCredentialEffects {
        self.proposal.effects()
    }
}

/// Commoncap remains a mandatory entry in the kernel-owned hook registry.
pub(in crate::task) fn authorize_commoncap_exec(
    context: &ExecCredentialSecurityContext<'_>,
) -> AxResult<()> {
    commoncap_exec_transition(context.proposal()).map_err(cred_error)
}

/// Fully authorized exec credential whose drop path is a complete abort.
pub(crate) struct PreparedExecCredential<'a> {
    prepared: PreparedCred<'a>,
    effects: ExecCredentialEffects,
    revalidation: ExecPtraceRevalidation,
}

impl<'a> PreparedExecCredential<'a> {
    pub(crate) const fn effects(&self) -> ExecCredentialEffects {
        self.effects
    }

    pub(crate) const fn revalidation(&self) -> ExecPtraceRevalidation {
        self.revalidation
    }

    pub(crate) fn into_prepared(self) -> PreparedCred<'a> {
        self.prepared
    }

    pub(crate) fn proposed_user_ns(&self) -> &Arc<UserNamespace> {
        self.prepared.proposed().user_ns()
    }
}

fn prepare_exec_update<'a>(
    update: CredentialUpdate<'a>,
    input: ExecCredentialInput,
    authorize: impl FnOnce(ExecCredentialSecurityContext<'_>) -> AxResult<()>,
) -> AxResult<PreparedExecCredential<'a>> {
    let proposal = derive_exec_credential(update.old_arc(), input).map_err(cred_error)?;
    authorize(ExecCredentialSecurityContext::new(&proposal))?;
    let effects = proposal.effects();
    let revalidation = proposal.revalidation();
    let prepared = update.finish_exec_proposal(proposal)?;
    Ok(PreparedExecCredential {
        prepared,
        effects,
        revalidation,
    })
}

impl Thread {
    pub(crate) fn prepare_exec_credential(
        &self,
        input: ExecCredentialInput,
    ) -> AxResult<PreparedExecCredential<'_>> {
        prepare_exec_update(self.credential.prepare(), input, |context| {
            super::security::dispatch_exec_credential(&context)
        })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec};

    use axerrno::AxError;
    use thekernel_linux_cred::{CAPABILITY_WORDS, GroupInfo};

    use super::*;
    use crate::task::{CredentialSlot, Credentials, Kgid, Kuid, creds::CapabilityState};

    fn unprivileged_slot() -> Arc<CredentialSlot> {
        let namespace = UserNamespace::try_new_root().unwrap();
        let root = Cred::try_root(namespace).unwrap();
        let slot = CredentialSlot::try_new(root).unwrap();
        let uid = Kuid::from_raw(1000).unwrap();
        let gid = Kgid::from_raw(1000).unwrap();
        let mut update = slot.prepare();
        update.builder.ids = Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        };
        update.builder.groups = GroupInfo::try_new(vec![gid]).unwrap();
        update.builder.caps = CapabilityState {
            effective: [0; CAPABILITY_WORDS],
            permitted: [0; CAPABILITY_WORDS],
            inheritable: [0; CAPABILITY_WORDS],
            bounding: thekernel_linux_cred::CAPABILITY_VALID_MASK,
            ambient: [0; CAPABILITY_WORDS],
            securebits: 0,
        };
        update.finish().unwrap().commit();
        slot
    }

    fn setuid_root_input() -> ExecCredentialInput {
        ExecCredentialInput::new(
            0o4000,
            Some(ExecFileOwner::new(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT)),
            ExecMountPrivilege::Honor,
            ExecTraceState::NotSuppressingPrivilege,
            ExecImageReadability::Readable,
            None,
        )
    }

    #[test]
    fn authorizer_observes_frozen_old_proposed_input_and_effects() {
        let slot = unprivileged_slot();
        let prepared = prepare_exec_update(slot.prepare(), setuid_root_input(), |context| {
            assert_eq!(context.old().ids().euid, Kuid::from_raw(1000).unwrap());
            assert_eq!(context.proposed().ids().euid, Kuid::INITIAL_ROOT);
            assert_eq!(context.input().mode() & 0o4000, 0o4000);
            assert!(context.effects().secure_exec());
            assert_eq!(
                context.effects().dumpability(),
                ExecDumpability::NotDumpable
            );
            Ok(())
        })
        .unwrap();
        assert!(prepared.effects().clear_pdeath_signal());
    }

    #[test]
    fn authorizer_denial_and_dropped_preparation_are_zero_effect_rollbacks() {
        let slot = unprivileged_slot();
        let old = slot.current();

        let denied = prepare_exec_update(slot.prepare(), setuid_root_input(), |_| {
            Err(AxError::OperationNotPermitted)
        });
        assert_eq!(denied.err(), Some(AxError::OperationNotPermitted));
        assert!(Arc::ptr_eq(&old, &slot.current()));

        let prepared =
            prepare_exec_update(slot.prepare(), setuid_root_input(), |_| Ok(())).unwrap();
        assert_eq!(
            prepared.effects().dumpability(),
            ExecDumpability::NotDumpable
        );
        drop(prepared);
        assert!(Arc::ptr_eq(&old, &slot.current()));
    }

    #[test]
    fn privileged_exec_revalidates_a_new_suppressing_ptrace_relation() {
        let slot = unprivileged_slot();
        let old = slot.current();
        let proposal =
            thekernel_linux_cred::derive_exec_credential(&old, setuid_root_input()).unwrap();
        assert!(
            proposal
                .revalidation()
                .is_stale(ExecTraceState::SuppressingPrivilege)
        );

        let already_suppressed = ExecCredentialInput::new(
            0o4000,
            Some(ExecFileOwner::new(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT)),
            ExecMountPrivilege::Honor,
            ExecTraceState::SuppressingPrivilege,
            ExecImageReadability::Readable,
            None,
        );
        let proposal =
            thekernel_linux_cred::derive_exec_credential(&old, already_suppressed).unwrap();
        assert!(
            !proposal
                .revalidation()
                .is_stale(ExecTraceState::SuppressingPrivilege)
        );
    }

    #[test]
    fn proposal_from_equal_looking_distinct_old_arc_is_rejected() {
        let first = unprivileged_slot();
        let first_old = first.current();
        let duplicate = thekernel_linux_cred::Credential::try_from_transition(
            &first_old,
            first_old.ids(),
            first_old.groups().clone(),
            first_old.capabilities(),
            first_old.no_new_privs(),
            first_old.user_ns().clone(),
            thekernel_linux_cred::CredentialTransitionMode::Normal,
        )
        .unwrap();
        assert!(!Arc::ptr_eq(&first_old, &duplicate));
        let second = CredentialSlot::try_new(duplicate.clone()).unwrap();
        let proposal =
            thekernel_linux_cred::derive_exec_credential(&first_old, setuid_root_input()).unwrap();

        let error = second
            .prepare()
            .finish_exec_proposal(proposal)
            .err()
            .unwrap();
        assert_eq!(error, AxError::OperationNotPermitted);
        assert!(Arc::ptr_eq(&first_old, &first.current()));
        assert!(Arc::ptr_eq(&duplicate, &second.current()));
    }
}
