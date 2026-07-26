//! Modules this kernel always admits.
//!
//! `CommoncapModule` carries the POSIX capability rules every other policy
//! composes with, so the registry builder cannot reach a usable state without
//! it. `NoopPolicyModule` occupies the second slot so multi-module ordering
//! stays exercised in every build rather than only where a policy happens to
//! be configured.

use super::*;

// Records what `CommoncapModule` observed at each commit point so tests can
// assert the transition rather than only its outcome.
#[cfg(test)]
std::thread_local! {
    static COMMONCAP_COMMIT_COUNT: Cell<u32> = const { Cell::new(0) };
    static COMMONCAP_COMMIT_OLD_UID: Cell<u32> = const { Cell::new(0) };
    static COMMONCAP_COMMIT_NEW_UID: Cell<u32> = const { Cell::new(0) };
    static COMMONCAP_COMMIT_TRANSITION: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
pub(in crate::task) fn reset_commoncap_post_commit_probe() {
    COMMONCAP_COMMIT_COUNT.with(|value| value.set(0));
    COMMONCAP_COMMIT_OLD_UID.with(|value| value.set(0));
    COMMONCAP_COMMIT_NEW_UID.with(|value| value.set(0));
    COMMONCAP_COMMIT_TRANSITION.with(|value| value.set(0));
}

#[cfg(test)]
pub(in crate::task) fn commoncap_post_commit_probe() -> (u32, u32, u32, u32) {
    (
        COMMONCAP_COMMIT_COUNT.with(Cell::get),
        COMMONCAP_COMMIT_OLD_UID.with(Cell::get),
        COMMONCAP_COMMIT_NEW_UID.with(Cell::get),
        COMMONCAP_COMMIT_TRANSITION.with(Cell::get),
    )
}

pub(crate) struct CommoncapModule;

impl SecurityModule for CommoncapModule {
    const KEY: ModuleKey = COMMONCAP_MODULE_KEY;
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        Ok(Self)
    }

    fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
        Ok(())
    }

    fn try_prepare_credential(
        &self,
        _old_credential: &CoreCred,
        _old_state: &Self::CredentialState,
        _proposed_credential: &CoreCred,
        _transition: CredentialStateTransition,
    ) -> AxResult<Self::CredentialState> {
        Ok(())
    }

    fn credential_committed(
        &self,
        context: CredentialPostCommitContext<'_, Self::CredentialState>,
    ) {
        #[cfg(test)]
        {
            assert_post_commit_callback_locks_released();
            COMMONCAP_COMMIT_COUNT.with(|value| value.set(value.get() + 1));
            COMMONCAP_COMMIT_OLD_UID
                .with(|value| value.set(context.old_credential().ids().ruid.into_raw()));
            COMMONCAP_COMMIT_NEW_UID
                .with(|value| value.set(context.new_credential().ids().ruid.into_raw()));
            let transition = match context.transition() {
                CredentialStateTransition::Fork => 1,
                CredentialStateTransition::Mutation(_) => 1 << 1,
                CredentialStateTransition::UserNamespace => 1 << 2,
                CredentialStateTransition::Exec => 1 << 3,
            };
            COMMONCAP_COMMIT_TRANSITION.with(|value| value.set(transition));
        }
        // Commoncap has no post-publication work today. Touch every immutable
        // fact here so the no-op implementation still exercises the complete
        // typed context in production builds.
        let _ = (
            context.old_credential(),
            context.old_state(),
            context.new_credential(),
            context.new_state(),
            context.transition(),
        );
    }

    fn capable(&self, context: &CoreCapabilitySecurityContext<'_>) -> AxResult<()> {
        // The ABI leaf has already executed commoncap and only exposes this
        // field-private context on success. Keep the mandatory first module in
        // the dispatch shape without repeating or weakening that decision.
        let _ = (
            context.actor(),
            context.target_user_ns(),
            context.capability(),
            context.operation(),
        );
        Ok(())
    }

    fn credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source_state: &Self::CredentialState,
        published_state: &Self::CredentialState,
    ) {
        #[cfg(test)]
        assert_post_commit_callback_locks_released();
        let _ = (
            context.source_credential(),
            context.published_credential(),
            context.source_user_ns(),
            context.target_user_ns(),
            context.target_object(),
            context.operation(),
            source_state,
            published_state,
        );
    }

    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        external_commoncap_ptrace_access(context.core()).map_err(authorization_error)
    }

    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        external_commoncap_ptrace_traceme(context.core()).map_err(authorization_error)
    }

    /// Validates the invariants that must still hold after commoncap's exec
    /// credential algebra has produced its proposed value. Keeping this in the
    /// mandatory module prevents an allow-by-default call-site closure from
    /// becoming the effective exec policy.
    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        authorize_commoncap_exec(context)
    }

    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        external_commoncap_scheduler(context.core()).map_err(authorization_error)
    }
}

/// A deliberately inert second module keeps stacked dispatch exercised in the
/// production shape without selecting a mandatory access-control policy.
pub(crate) struct NoopPolicyModule;

impl SecurityModule for NoopPolicyModule {
    const KEY: ModuleKey = NOOP_POLICY_MODULE_KEY;
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        Ok(Self)
    }

    fn try_init_credential(&self, _credential: &CoreCred) -> AxResult<Self::CredentialState> {
        Ok(())
    }

    fn try_prepare_credential(
        &self,
        _old_credential: &CoreCred,
        _old_state: &Self::CredentialState,
        _proposed_credential: &CoreCred,
        _transition: CredentialStateTransition,
    ) -> AxResult<Self::CredentialState> {
        Ok(())
    }

    fn capable(&self, _context: &CoreCapabilitySecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn credential_published(
        &self,
        _context: &CoreCredentialPublicationContext<'_>,
        _source_state: &Self::CredentialState,
        _published_state: &Self::CredentialState,
    ) {
    }
}
