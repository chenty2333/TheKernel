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
    creds::{CoreCred, Cred, CredentialUpdate, PreparedCred},
    security::{
        CommittingExecSecurity, CredentialSecurityState, CredentialStateTransition,
        PendingExecSecurity,
    },
};
use crate::file::executable::CredentialReadLease;

/// Stable filesystem identity carried through one exec security decision.
///
/// The value contains no pathname or VFS handle. Hooks therefore cannot repeat
/// lookup or retain a mutable filesystem object after the loader releases it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecFileIdentity {
    device: u64,
    inode: u64,
}

impl ExecFileIdentity {
    pub(crate) const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    pub(crate) const fn device(self) -> u64 {
        self.device
    }

    pub(crate) const fn inode(self) -> u64 {
        self.inode
    }
}

/// The role played by one executable component in a single exec chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecExecutableRole {
    /// The object named directly by execve/execveat.
    Requested,
    /// A `#!` interpreter selected by one binary-format rewrite.
    ScriptInterpreter,
    /// An ELF `PT_INTERP` dynamic linker mapped into the new image.
    DynamicLinker,
}

/// Immutable, lookup-free executable facts delivered to typed hooks.
#[derive(Clone)]
pub(crate) struct ExecFileSecurityObject {
    identity: ExecFileIdentity,
    owner_user_ns: Arc<UserNamespace>,
    owner: Option<ExecFileOwner>,
    mode: u16,
    readable: bool,
    role: ExecExecutableRole,
}

/// Opaque identity for the new address-space generation named by an exec
/// lifecycle callback. The process layer owns the backing `Arc` throughout
/// both phases; hooks receive only this non-dereferenceable stable value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecImageIdentity(usize);

impl ExecImageIdentity {
    pub(crate) fn from_arc<T>(image: &Arc<T>) -> Self {
        Self(Arc::as_ptr(image) as usize)
    }

    pub(crate) const fn as_usize(self) -> usize {
        self.0
    }
}

/// Process/image facts frozen before the exec point of no return.
#[derive(Clone)]
pub(crate) struct ExecCommitRuntime {
    process_id: u32,
    executing_tid: u32,
    post_exec_tid: u32,
    image_identity: ExecImageIdentity,
    image_owner_user_ns: Arc<UserNamespace>,
}

impl ExecCommitRuntime {
    pub(crate) fn new(
        process_id: u32,
        executing_tid: u32,
        post_exec_tid: u32,
        image_identity: ExecImageIdentity,
        image_owner_user_ns: Arc<UserNamespace>,
    ) -> Self {
        Self {
            process_id,
            executing_tid,
            post_exec_tid,
            image_identity,
            image_owner_user_ns,
        }
    }

    pub(crate) const fn process_id(&self) -> u32 {
        self.process_id
    }

    pub(crate) const fn executing_tid(&self) -> u32 {
        self.executing_tid
    }

    pub(crate) const fn post_exec_tid(&self) -> u32 {
        self.post_exec_tid
    }

    pub(crate) const fn image_identity(&self) -> ExecImageIdentity {
        self.image_identity
    }

    pub(crate) const fn image_owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.image_owner_user_ns
    }
}

impl ExecFileSecurityObject {
    pub(crate) const fn new(
        identity: ExecFileIdentity,
        owner_user_ns: Arc<UserNamespace>,
        owner: Option<ExecFileOwner>,
        mode: u16,
        readable: bool,
        role: ExecExecutableRole,
    ) -> Self {
        Self {
            identity,
            owner_user_ns,
            owner,
            mode,
            readable,
            role,
        }
    }

    pub(crate) const fn identity(&self) -> ExecFileIdentity {
        self.identity
    }

    pub(crate) const fn owner_user_ns(&self) -> &Arc<UserNamespace> {
        &self.owner_user_ns
    }

    pub(crate) const fn owner(&self) -> Option<ExecFileOwner> {
        self.owner
    }

    pub(crate) const fn mode(&self) -> u16 {
        self.mode
    }

    pub(crate) const fn readable(&self) -> bool {
        self.readable
    }

    pub(crate) const fn role(&self) -> ExecExecutableRole {
        self.role
    }
}

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

/// Kernel-owned exec proposal. It binds the ABI crate's exact old core to the
/// exact old outer composite and carries every fully prepared module state.
pub(in crate::task) struct ExecCredentialDraft {
    expected_old: Arc<Cred>,
    proposal: ExecCredentialProposal<UserNamespace>,
    proposed_security: CredentialSecurityState,
    source: ExecFileSecurityObject,
}

impl ExecCredentialDraft {
    pub(in crate::task) fn try_new(
        old: &Arc<Cred>,
        input: ExecCredentialInput,
        source: ExecFileSecurityObject,
    ) -> AxResult<Self> {
        let proposal = derive_exec_credential(old.core_arc(), input).map_err(cred_error)?;
        let proposed_security = Cred::try_prepare_security_transition(
            old,
            proposal.proposed(),
            CredentialStateTransition::Exec,
        )?;
        Ok(Self {
            expected_old: old.clone(),
            proposal,
            proposed_security,
            source,
        })
    }

    pub(in crate::task) fn old(&self) -> &Cred {
        &self.expected_old
    }

    pub(in crate::task) fn proposal(&self) -> &ExecCredentialProposal<UserNamespace> {
        &self.proposal
    }

    pub(in crate::task) fn proposed_core(&self) -> &CoreCred {
        self.proposal.proposed()
    }

    pub(in crate::task) fn proposed_security(&self) -> &CredentialSecurityState {
        &self.proposed_security
    }

    pub(in crate::task) fn source(&self) -> &ExecFileSecurityObject {
        &self.source
    }

    pub(in crate::task) fn try_into_parts(
        self,
        expected_old: &Arc<Cred>,
    ) -> AxResult<(Arc<CoreCred>, CredentialSecurityState)> {
        if !Arc::ptr_eq(&self.expected_old, expected_old) {
            return Err(axerrno::AxError::OperationNotPermitted);
        }
        let core = self
            .proposal
            .try_into_proposed(expected_old.core_arc())
            .map_err(cred_error)?;
        Ok((core, self.proposed_security))
    }
}

/// Typed authorization view over one opaque, exact-old-bound draft.
///
/// The draft is only borrowed. A hook cannot extract its proposed core/state
/// owners or publish them; successful authorization returns control to the
/// writer transaction, which consumes both exact-old checks.
pub(crate) struct ExecCredentialSecurityContext<'a> {
    draft: &'a ExecCredentialDraft,
}

impl<'a> ExecCredentialSecurityContext<'a> {
    pub(in crate::task) fn new(draft: &'a ExecCredentialDraft) -> Self {
        Self { draft }
    }

    pub(in crate::task) fn proposal(&self) -> &ExecCredentialProposal<UserNamespace> {
        self.draft.proposal()
    }

    pub(in crate::task) fn old(&self) -> &Cred {
        self.draft.old()
    }

    pub(in crate::task) fn proposed(&self) -> &CoreCred {
        self.draft.proposed_core()
    }

    pub(in crate::task) fn draft(&self) -> &ExecCredentialDraft {
        self.draft
    }

    pub(in crate::task) fn input(&self) -> ExecCredentialInput {
        self.draft.proposal().input()
    }

    pub(in crate::task) fn effects(&self) -> ExecCredentialEffects {
        self.draft.proposal().effects()
    }

    pub(crate) fn source(&self) -> &ExecFileSecurityObject {
        self.draft.source()
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
    security: PendingExecSecurity,
}

impl<'a> PreparedExecCredential<'a> {
    pub(crate) const fn effects(&self) -> ExecCredentialEffects {
        self.effects
    }

    pub(crate) const fn revalidation(&self) -> ExecPtraceRevalidation {
        self.revalidation
    }

    pub(crate) fn proposed_user_ns(&self) -> &Arc<UserNamespace> {
        self.prepared.proposed().user_ns()
    }

    /// Crosses the exec point of no return after all fallible work and final
    /// relationship revalidation have completed. The returned type is the only
    /// credential accepted by composite image publication.
    pub(crate) fn begin_commit(
        self,
        credential_lease: CredentialReadLease,
        runtime: ExecCommitRuntime,
    ) -> CommittingExecCredential<'a> {
        let Self {
            prepared,
            effects,
            revalidation: _,
            security,
        } = self;
        CommittingExecCredential {
            prepared,
            effects,
            security: security.committing(runtime),
            credential_lease,
        }
    }
}

/// Exec credential after the infallible committing notification has run.
pub(crate) struct CommittingExecCredential<'a> {
    prepared: PreparedCred<'a>,
    effects: ExecCredentialEffects,
    security: CommittingExecSecurity,
    credential_lease: CredentialReadLease,
}

impl<'a> CommittingExecCredential<'a> {
    pub(in crate::task) fn into_parts(
        self,
    ) -> (
        PreparedCred<'a>,
        ExecCredentialEffects,
        CommittingExecSecurity,
        CredentialReadLease,
    ) {
        (
            self.prepared,
            self.effects,
            self.security,
            self.credential_lease,
        )
    }
}

fn prepare_exec_update<'a>(
    update: CredentialUpdate<'a>,
    input: ExecCredentialInput,
    source: ExecFileSecurityObject,
    authorize: impl FnOnce(ExecCredentialSecurityContext<'_>) -> AxResult<()>,
) -> AxResult<PreparedExecCredential<'a>> {
    let draft = ExecCredentialDraft::try_new(update.old_arc(), input, source)?;
    authorize(ExecCredentialSecurityContext::new(&draft))?;
    let effects = draft.proposal().effects();
    let revalidation = draft.proposal().revalidation();
    let source = draft.source().clone();
    let prepared = update.finish_exec_draft(draft)?;
    let security = PendingExecSecurity::try_new(&prepared, source, effects)?;
    Ok(PreparedExecCredential {
        prepared,
        effects,
        revalidation,
        security,
    })
}

fn prepare_exec_update_exact<'a>(
    update: CredentialUpdate<'a>,
    expected_old: &Arc<Cred>,
    input: ExecCredentialInput,
    source: ExecFileSecurityObject,
    authorize: impl FnOnce(ExecCredentialSecurityContext<'_>) -> AxResult<()>,
) -> AxResult<PreparedExecCredential<'a>> {
    if !Arc::ptr_eq(update.old_arc(), expected_old) {
        return Err(axerrno::AxError::Interrupted);
    }
    prepare_exec_update(update, input, source, authorize)
}

impl Thread {
    pub(crate) fn prepare_exec_credential(
        &self,
        expected_old: &Arc<Cred>,
        input: ExecCredentialInput,
        source: ExecFileSecurityObject,
    ) -> AxResult<PreparedExecCredential<'_>> {
        prepare_exec_update_exact(
            self.credential.prepare(),
            expected_old,
            input,
            source,
            |context| super::security::dispatch_exec_credential(&context),
        )
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, vec};
    use core::cell::Cell;

    use axerrno::AxError;
    use thekernel_linux_cred::{CAPABILITY_WORDS, GroupInfo};

    use super::*;
    use crate::task::{
        CredentialSlot, Credentials, Kgid, Kuid,
        creds::CapabilityState,
        security::{commoncap_post_commit_probe, reset_commoncap_post_commit_probe},
    };

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

    fn exec_source(credential: &Arc<Cred>) -> ExecFileSecurityObject {
        ExecFileSecurityObject::new(
            ExecFileIdentity::new(7, 11),
            credential.user_ns().clone(),
            Some(ExecFileOwner::new(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT)),
            0o755,
            true,
            ExecExecutableRole::Requested,
        )
    }

    #[test]
    fn authorizer_observes_frozen_old_proposed_input_and_effects() {
        let slot = unprivileged_slot();
        let old = slot.current();
        let prepared = prepare_exec_update(
            slot.prepare(),
            setuid_root_input(),
            exec_source(&old),
            |context| {
                assert_eq!(context.old().ids().euid, Kuid::from_raw(1000).unwrap());
                assert_eq!(context.proposed().ids().euid, Kuid::INITIAL_ROOT);
                assert_eq!(context.input().mode() & 0o4000, 0o4000);
                assert!(context.effects().secure_exec());
                assert_eq!(
                    context.effects().dumpability(),
                    ExecDumpability::NotDumpable
                );
                assert_eq!(context.source().identity(), ExecFileIdentity::new(7, 11));
                Ok(())
            },
        )
        .unwrap();
        assert!(prepared.effects().clear_pdeath_signal());
    }

    #[test]
    fn authorizer_denial_and_dropped_preparation_are_zero_effect_rollbacks() {
        let slot = unprivileged_slot();
        let old = slot.current();
        reset_commoncap_post_commit_probe();

        let denied = prepare_exec_update(
            slot.prepare(),
            setuid_root_input(),
            exec_source(&old),
            |_| Err(AxError::OperationNotPermitted),
        );
        assert_eq!(denied.err(), Some(AxError::OperationNotPermitted));
        assert!(Arc::ptr_eq(&old, &slot.current()));

        let prepared = prepare_exec_update(
            slot.prepare(),
            setuid_root_input(),
            exec_source(&old),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(
            prepared.effects().dumpability(),
            ExecDumpability::NotDumpable
        );
        drop(prepared);
        assert!(Arc::ptr_eq(&old, &slot.current()));
        assert_eq!(commoncap_post_commit_probe().0, 0);
    }

    #[test]
    fn privileged_exec_revalidates_a_new_suppressing_ptrace_relation() {
        let slot = unprivileged_slot();
        let old = slot.current();
        reset_commoncap_post_commit_probe();
        let proposal =
            thekernel_linux_cred::derive_exec_credential(old.core_arc(), setuid_root_input())
                .unwrap();
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
            thekernel_linux_cred::derive_exec_credential(old.core_arc(), already_suppressed)
                .unwrap();
        assert!(
            !proposal
                .revalidation()
                .is_stale(ExecTraceState::SuppressingPrivilege)
        );

        let prepared = prepare_exec_update(
            slot.prepare(),
            setuid_root_input(),
            exec_source(&old),
            |_| Ok(()),
        )
        .unwrap();
        assert!(
            prepared
                .revalidation()
                .is_stale(ExecTraceState::SuppressingPrivilege)
        );
        drop(prepared);
        assert_eq!(commoncap_post_commit_probe().0, 0);
    }

    #[test]
    fn draft_from_shared_core_distinct_outer_credential_is_rejected() {
        let first = unprivileged_slot();
        let first_old = first.current();
        let duplicate = Cred::try_clone_for_fork(&first_old).unwrap();
        assert!(!Arc::ptr_eq(&first_old, &duplicate));
        assert!(Arc::ptr_eq(first_old.core_arc(), duplicate.core_arc()));
        let second = CredentialSlot::try_new(duplicate.clone()).unwrap();
        let draft =
            ExecCredentialDraft::try_new(&first_old, setuid_root_input(), exec_source(&first_old))
                .unwrap();

        let error = second.prepare().finish_exec_draft(draft).err().unwrap();
        assert_eq!(error, AxError::OperationNotPermitted);
        assert!(Arc::ptr_eq(&first_old, &first.current()));
        assert!(Arc::ptr_eq(&duplicate, &second.current()));
    }

    #[test]
    fn stale_loader_actor_is_interrupted_before_proposal_or_hook_dispatch() {
        let slot = unprivileged_slot();
        let current = slot.current();
        let stale = Cred::try_clone_for_fork(&current).unwrap();
        assert!(!Arc::ptr_eq(&current, &stale));
        assert!(Arc::ptr_eq(current.core_arc(), stale.core_arc()));
        let hook_ran = Cell::new(false);

        let result = prepare_exec_update_exact(
            slot.prepare(),
            &stale,
            setuid_root_input(),
            exec_source(&stale),
            |_| {
                hook_ran.set(true);
                Ok(())
            },
        );
        assert_eq!(result.err(), Some(AxError::Interrupted));
        assert!(!hook_ran.get());
        assert!(Arc::ptr_eq(&current, &slot.current()));
    }
}
