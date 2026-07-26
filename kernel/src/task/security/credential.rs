//! Per-credential security state and its publication transitions.
//!
//! State is prepared under fallible allocation and committed separately, so a
//! partial preparation is always rollbackable. The exec transition is staged
//! through distinct pending, committing, and completed types so a
//! half-applied exec cannot be observed or dropped into an ambiguous state.

use super::*;

/// Complete immutable per-module state carried by one composite credential.
/// The layout identity and dense ModuleId order are checked before every
/// prepare/authorize pass, so a foreign or malformed state fails closed.
pub(in crate::task) struct CredentialSecurityState {
    pub(super) registry: FrozenSecurityRegistry,
    pub(super) slots: Vec<OwnedModuleCredState>,
    pub(super) identity: Arc<CredentialStateIdentity>,
    pub(super) derivation: CredentialStateDerivation,
}

impl CredentialSecurityState {
    pub(in crate::task) fn registry(&self) -> FrozenSecurityRegistry {
        self.registry
    }

    pub(super) fn validate_for(&self, registry: FrozenSecurityRegistry) -> AxResult<()> {
        if !self.registry.same_registry(registry)
            || self.slots.len() != registry.registry().modules.len()
        {
            return Err(AxError::OperationNotPermitted);
        }
        for (index, slot) in self.slots.iter().enumerate() {
            if usize::from(slot.module_id.0) != index
                || registry.registry().modules[index].id != slot.module_id
            {
                return Err(AxError::OperationNotPermitted);
            }
        }
        Ok(())
    }

    pub(super) fn validate_live(&self) -> AxResult<()> {
        if self.identity.live.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(AxError::OperationNotPermitted)
        }
    }

    pub(super) fn prepared_transition_from(
        &self,
        source: &CredentialSecurityState,
    ) -> AxResult<CredentialStateTransition> {
        let CredentialStateDerivation::Prepared {
            source: expected_source,
            transition: expected_transition,
        } = &self.derivation
        else {
            return Err(AxError::BadState);
        };
        if !Arc::ptr_eq(expected_source, &source.identity)
            || Arc::ptr_eq(&self.identity, &source.identity)
        {
            return Err(AxError::BadState);
        }
        Ok(*expected_transition)
    }

    pub(super) fn claim_transition_from(
        &self,
        source: &CredentialSecurityState,
        transition: CredentialStateTransition,
    ) -> AxResult<()> {
        if self.prepared_transition_from(source)? != transition {
            return Err(AxError::BadState);
        }
        self.identity
            .publication_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| AxError::BadState)
    }

    pub(super) fn activate_claimed(&self) {
        assert!(
            self.identity.publication_claimed.load(Ordering::Acquire),
            "credential state activated without a publication claim"
        );
        self.identity
            .live
            .compare_exchange(false, true, Ordering::Release, Ordering::Acquire)
            .expect("credential state activated more than once");
    }

    #[cfg(test)]
    pub(in crate::task) fn activate_fixture(&self) {
        self.identity
            .publication_claimed
            .store(true, Ordering::Release);
        self.identity.live.store(true, Ordering::Release);
    }
}

impl Drop for CredentialSecurityState {
    fn drop(&mut self) {
        while self.slots.pop().is_some() {}
    }
}

/// Linear, pre-publication proof for one post-commit notification pass.
///
/// The exact old/new composite credentials are retained by this token, so a
/// later writer cannot make the callback observe a different transition. A
/// dropped unpublished token is an ordinary abort and emits no notification.
#[must_use = "a published credential must consume its post-commit notification token"]
pub(in crate::task) struct PendingCredentialPostCommit {
    pub(super) registry: FrozenSecurityRegistry,
    pub(super) old: Arc<Cred>,
    pub(super) new: Arc<Cred>,
    pub(super) transition: CredentialStateTransition,
}

impl PendingCredentialPostCommit {
    pub(in crate::task) fn try_new(
        old: &Arc<Cred>,
        new: &Arc<Cred>,
        transition: CredentialStateTransition,
    ) -> AxResult<Self> {
        if !matches!(
            transition,
            CredentialStateTransition::Mutation(_) | CredentialStateTransition::Exec
        ) {
            return Err(AxError::BadState);
        }
        let registry = old.security().registry();
        registry.registry().validate_credential_pair(old, new)?;
        new.security()
            .claim_transition_from(old.security(), transition)?;
        Ok(Self {
            registry,
            old: old.clone(),
            new: new.clone(),
            transition,
        })
    }

    pub(in crate::task) fn activate(&self) {
        self.new.security().activate_claimed();
    }

    pub(in crate::task) fn notify(self) {
        let Self {
            registry,
            old,
            new,
            transition,
        } = self;
        debug_assert!(new.security().validate_live().is_ok());
        registry
            .registry()
            .notify_credential_committed(&old, &new, transition);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialPublicationKind {
    Fork,
    UserNamespace,
}

/// Linear pre-publication ownership for a separately prepared child
/// credential and its exact target object.
///
/// Both composite state vectors and their creating runtimes are validated
/// before this token is returned. Dropping it while the child is still private
/// is the ordinary rollback path and emits no callback. Once TASK_TABLE has
/// committed and all publication locks are released, [`Self::notify`] consumes
/// the token, constructs the external success-only context, and performs one
/// infallible frozen-order notification pass.
#[must_use = "an admitted child publication must either abort or notify after visibility"]
pub(crate) struct PendingCredentialPublication<T: CredentialPublicationTargetOwner> {
    pub(super) registry: FrozenSecurityRegistry,
    pub(super) source: Arc<Cred>,
    pub(super) published: Arc<Cred>,
    pub(super) target_owner: T,
    pub(super) kind: CredentialPublicationKind,
}

impl<T: CredentialPublicationTargetOwner> PendingCredentialPublication<T> {
    pub(crate) fn try_fork(
        source: &Arc<Cred>,
        published: &Arc<Cred>,
        target_owner: T,
    ) -> AxResult<Self> {
        if !source.same_linux_credential(published) {
            return Err(AxError::BadState);
        }
        Self::try_new(
            source,
            published,
            target_owner,
            CredentialPublicationKind::Fork,
        )
    }

    pub(crate) fn try_user_namespace(
        source: &Arc<Cred>,
        published: &Arc<Cred>,
        target_owner: T,
    ) -> AxResult<Self> {
        let published_parent = published.user_ns().parent();
        if source.same_linux_credential(published)
            || !published_parent
                .as_ref()
                .is_some_and(|parent| Arc::ptr_eq(parent, source.user_ns()))
        {
            return Err(AxError::BadState);
        }
        Self::try_new(
            source,
            published,
            target_owner,
            CredentialPublicationKind::UserNamespace,
        )
    }

    pub(super) fn try_new(
        source: &Arc<Cred>,
        published: &Arc<Cred>,
        target_owner: T,
        kind: CredentialPublicationKind,
    ) -> AxResult<Self> {
        let registry = source.security().registry();
        registry
            .registry()
            .validate_credential_pair(source, published)?;
        let transition = match kind {
            CredentialPublicationKind::Fork => CredentialStateTransition::Fork,
            CredentialPublicationKind::UserNamespace => CredentialStateTransition::UserNamespace,
        };
        published
            .security()
            .claim_transition_from(source.security(), transition)?;
        Ok(Self {
            registry,
            source: source.clone(),
            published: published.clone(),
            target_owner,
            kind,
        })
    }

    /// Makes the fully prepared state usable immediately before the first
    /// externally visible child-identity publication.
    pub(crate) fn activate(&self) {
        self.published.security().activate_claimed();
    }

    /// Delivers the successful-publication event. The caller owns the external
    /// visibility linearization point and must invoke this only after releasing
    /// TASK_TABLE, task-parent, and process-lifecycle publication locks.
    pub(crate) fn notify(self) {
        let Self {
            registry,
            source,
            published,
            target_owner,
            kind,
        } = self;
        debug_assert!(published.security().validate_live().is_ok());
        let target = target_owner.credential_publication_target();
        let context = match kind {
            CredentialPublicationKind::Fork => {
                thekernel_linux_cred::CredentialPublicationContext::fork(
                    source.core(),
                    published.core(),
                    &target,
                )
            }
            CredentialPublicationKind::UserNamespace => {
                thekernel_linux_cred::CredentialPublicationContext::user_namespace(
                    source.core(),
                    published.core(),
                    &target,
                )
            }
        };
        registry
            .registry()
            .notify_credential_published(&context, &source, &published);
        drop(target_owner);
    }
}

/// Linear preflight for the exec-only committing/committed hook pair.
///
/// It retains the exact old/new credentials and terminal credential source.
/// Dropping this value before `committing()` is a normal exec rollback and
/// emits no lifecycle notification.
#[must_use = "an admitted exec must either abort or enter the committing phase"]
pub(in crate::task) struct PendingExecSecurity {
    pub(super) registry: FrozenSecurityRegistry,
    pub(super) old: Arc<Cred>,
    pub(super) new: Arc<Cred>,
    pub(super) source: ExecFileSecurityObject,
    pub(super) effects: ExecCredentialEffects,
}

impl PendingExecSecurity {
    pub(in crate::task) fn try_new(
        prepared: &PreparedCred<'_>,
        source: ExecFileSecurityObject,
        effects: ExecCredentialEffects,
    ) -> AxResult<Self> {
        let old = prepared.old_arc();
        let new = prepared.proposed_arc();
        let registry = old.security().registry();
        registry.registry().validate_credential_pair(old, new)?;
        Ok(Self {
            registry,
            old: old.clone(),
            new: new.clone(),
            source,
            effects,
        })
    }

    pub(in crate::task) fn committing(self, runtime: ExecCommitRuntime) -> CommittingExecSecurity {
        let context = ExecCommittingSecurityContext::new(
            &self.old,
            &self.new,
            &self.source,
            self.effects,
            &runtime,
        );
        self.registry.registry().notify_exec_committing(&context);
        CommittingExecSecurity {
            registry: self.registry,
            old: Some(self.old),
            new: Some(self.new),
            source: Some(self.source),
            effects: self.effects,
            runtime: Some(runtime),
            armed: true,
        }
    }
}

/// Exec lifecycle after the point-of-no-return callback and before the
/// matching committed callback. Composite image publication carries this
/// token so committed notification cannot be forgotten or detached from the
/// exact credentials which were installed.
#[must_use = "a committing exec must complete its committed notification"]
pub(in crate::task) struct CommittingExecSecurity {
    pub(super) registry: FrozenSecurityRegistry,
    pub(super) old: Option<Arc<Cred>>,
    pub(super) new: Option<Arc<Cred>>,
    pub(super) source: Option<ExecFileSecurityObject>,
    pub(super) effects: ExecCredentialEffects,
    pub(super) runtime: Option<ExecCommitRuntime>,
    pub(super) armed: bool,
}

impl CommittingExecSecurity {
    pub(in crate::task) fn committed(mut self) -> CompletedExecSecurity {
        let old = self
            .old
            .take()
            .expect("committing exec old credential is live");
        let new = self
            .new
            .take()
            .expect("committing exec new credential is live");
        let source = self.source.take().expect("committing exec source is live");
        let runtime = self
            .runtime
            .take()
            .expect("committing exec runtime facts are live");
        let context =
            ExecCommittedSecurityContext::new(&old, &new, &source, self.effects, &runtime);
        self.registry.registry().notify_exec_committed(&context);
        self.armed = false;
        CompletedExecSecurity {
            _old: old,
            _new: new,
            _source: source,
            _runtime: runtime,
        }
    }
}

impl Drop for CommittingExecSecurity {
    fn drop(&mut self) {
        assert!(
            !self.armed,
            "committing exec dropped without committed security notification"
        );
    }
}

/// Exact exec security facts retained after the full-image callback until the
/// caller has released the exec and vfork publication gates.
#[must_use = "completed exec security ownership must reach the retirement boundary"]
pub(in crate::task) struct CompletedExecSecurity {
    pub(super) _old: Arc<Cred>,
    pub(super) _new: Arc<Cred>,
    pub(super) _source: ExecFileSecurityObject,
    pub(super) _runtime: ExecCommitRuntime,
}
