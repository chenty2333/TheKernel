//! Fallible registry construction, immutable publication, and dispatch.
//!
//! Modules are admitted as complete units while the registry is still
//! private, then frozen and published exactly once before the initial
//! credential exists. After publication the registry is immutable: dispatch
//! walks the recorded declaration order and cannot allocate, register,
//! remove, or silently skip a module.

use super::*;

pub(crate) struct RegisteredModule {
    pub(super) id: ModuleId,
    pub(super) key: ModuleKey,
    pub(super) module: Arc<dyn ErasedSecurityModule>,
}

pub(crate) struct NeedsCommoncap;
pub(crate) struct HasCommoncap;

/// Fallible, bounded boot builder. Only `HasCommoncap` can be frozen.
pub(crate) struct SecurityRegistryBuilder<State> {
    pub(super) modules: Option<Vec<RegisteredModule>>,
    pub(super) _state: PhantomData<State>,
}

impl SecurityRegistryBuilder<NeedsCommoncap> {
    pub(super) fn try_new() -> Result<Self, RegistryBuildError> {
        Self::try_new_with_reservation(SECURITY_MODULE_LIMIT)
    }

    pub(super) fn try_new_with_reservation(reservation: usize) -> Result<Self, RegistryBuildError> {
        let mut modules = Vec::new();
        modules
            .try_reserve_exact(reservation.max(SECURITY_MODULE_LIMIT))
            .map_err(|_| RegistryBuildError::NoMemory)?;
        Ok(Self {
            modules: Some(modules),
            _state: PhantomData,
        })
    }

    pub(super) fn try_register_commoncap(
        self,
    ) -> Result<SecurityRegistryBuilder<HasCommoncap>, RegistryBuildError> {
        self.try_register_commoncap_with(CommoncapModule::try_boot_init)
    }

    pub(super) fn try_register_commoncap_with<F>(
        mut self,
        init: F,
    ) -> Result<SecurityRegistryBuilder<HasCommoncap>, RegistryBuildError>
    where
        F: FnOnce() -> Result<CommoncapModule, RegistryBuildError>,
    {
        debug_assert!(self.modules().is_empty());
        let module = init()?;
        self.push_commoncap(module)?;
        let modules = self.modules.take();
        Ok(SecurityRegistryBuilder {
            modules,
            _state: PhantomData,
        })
    }

    pub(super) fn push_commoncap(
        &mut self,
        module: CommoncapModule,
    ) -> Result<ModuleId, RegistryBuildError> {
        try_push_registered_module(&mut self.modules, module, try_allocate_security_module)
    }
}

impl SecurityRegistryBuilder<HasCommoncap> {
    pub(super) fn try_register<M: SecurityModule>(
        &mut self,
    ) -> Result<ModuleId, RegistryBuildError> {
        self.validate_registration(M::KEY)?;
        let module = M::try_boot_init()?;
        self.push_prevalidated(module)
    }

    #[cfg(test)]
    pub(super) fn try_register_initialized<M: SecurityModule>(
        &mut self,
        module: M,
    ) -> Result<ModuleId, RegistryBuildError> {
        self.validate_registration(M::KEY)?;
        self.push_prevalidated(module)
    }

    #[cfg(test)]
    pub(super) fn try_register_with_allocator<M, F>(
        &mut self,
        allocate: F,
    ) -> Result<ModuleId, RegistryBuildError>
    where
        M: SecurityModule,
        F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
    {
        self.validate_registration(M::KEY)?;
        let module = M::try_boot_init()?;
        self.push_prevalidated_with(module, allocate)
    }

    pub(super) fn validate_registration(&self, key: ModuleKey) -> Result<(), RegistryBuildError> {
        if key == COMMONCAP_MODULE_KEY {
            return Err(RegistryBuildError::ReservedModuleKey);
        }
        if self.modules().iter().any(|module| module.key == key) {
            return Err(RegistryBuildError::DuplicateModule);
        }
        if self.modules().len() >= SECURITY_MODULE_LIMIT {
            return Err(RegistryBuildError::Capacity);
        }
        Ok(())
    }

    pub(super) fn push_prevalidated<M: SecurityModule>(
        &mut self,
        module: M,
    ) -> Result<ModuleId, RegistryBuildError> {
        self.push_prevalidated_with(module, try_allocate_security_module)
    }

    pub(super) fn push_prevalidated_with<M, F>(
        &mut self,
        module: M,
        allocate: F,
    ) -> Result<ModuleId, RegistryBuildError>
    where
        M: SecurityModule,
        F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
    {
        try_push_registered_module(&mut self.modules, module, allocate)
    }

    pub(super) fn freeze(mut self) -> SecurityRegistry {
        let modules = self.modules.take().expect("registry builder was consumed");
        debug_assert!(!modules.is_empty());
        debug_assert_eq!(modules[0].key, COMMONCAP_MODULE_KEY);
        SecurityRegistry { modules }
    }
}

impl<State> SecurityRegistryBuilder<State> {
    pub(super) fn modules(&self) -> &[RegisteredModule] {
        self.modules
            .as_deref()
            .expect("registry builder was consumed")
    }
}

pub(super) fn try_allocate_security_module<M: SecurityModule>(
    module: M,
) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError> {
    let module: Arc<dyn ErasedSecurityModule> =
        Arc::try_new(module).map_err(|_| RegistryBuildError::NoMemory)?;
    Ok(module)
}

pub(super) fn try_push_registered_module<M, F>(
    modules: &mut Option<Vec<RegisteredModule>>,
    module: M,
    allocate: F,
) -> Result<ModuleId, RegistryBuildError>
where
    M: SecurityModule,
    F: FnOnce(M) -> Result<Arc<dyn ErasedSecurityModule>, RegistryBuildError>,
{
    let modules = modules.as_mut().expect("registry builder was consumed");
    debug_assert!(modules.len() < SECURITY_MODULE_LIMIT);
    debug_assert!(modules.len() < modules.capacity());
    let id = ModuleId(u8::try_from(modules.len()).expect("bounded module index fits u8"));
    let key = M::KEY;
    let module = allocate(module)?;
    modules.push(RegisteredModule { id, key, module });
    Ok(id)
}

impl<State> Drop for SecurityRegistryBuilder<State> {
    fn drop(&mut self) {
        if let Some(modules) = &mut self.modules {
            while modules.pop().is_some() {}
        }
    }
}

/// Immutable, allocation-free runtime dispatch table.
pub(crate) struct SecurityRegistry {
    pub(super) modules: Vec<RegisteredModule>,
}

pub(crate) struct OwnedModuleCredState {
    pub(super) module_id: ModuleId,
    pub(super) erased: Box<dyn ErasedOwnedCredentialState>,
}

pub(crate) struct CredentialStateIdentity {
    pub(super) publication_claimed: AtomicBool,
    pub(super) live: AtomicBool,
}

pub(super) enum CredentialStateDerivation {
    Initial,
    Prepared {
        source: Arc<CredentialStateIdentity>,
        transition: CredentialStateTransition,
    },
}

impl SecurityRegistry {
    pub(super) fn validate_erased_slots(&self, slots: &[OwnedModuleCredState]) -> AxResult<()> {
        if slots.len() != self.modules.len() {
            return Err(AxError::OperationNotPermitted);
        }
        for (registered, slot) in self.modules.iter().zip(slots) {
            if registered.id != slot.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .validate_credential_state(slot.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn security_slots<'a>(
        &self,
        security: &'a CredentialSecurityState,
    ) -> AxResult<&'a [OwnedModuleCredState]> {
        let registry = security.registry();
        if !core::ptr::eq(self, registry.registry()) {
            return Err(AxError::OperationNotPermitted);
        }
        security.validate_for(registry)?;
        self.validate_erased_slots(&security.slots)?;
        Ok(&security.slots)
    }

    pub(super) fn credential_slots<'a>(
        &self,
        credential: &'a Cred,
    ) -> AxResult<&'a [OwnedModuleCredState]> {
        credential.security().validate_live()?;
        self.security_slots(credential.security())
    }

    /// Performs the complete fallible layout, ModuleId, erased-type, and
    /// exact-runtime validation for both composites before publication. No
    /// module callback has run if either (including a late) slot is malformed.
    pub(super) fn validate_credential_pair(&self, source: &Cred, published: &Cred) -> AxResult<()> {
        self.credential_slots(source)?;
        self.security_slots(published.security())?;
        Ok(())
    }

    /// Dispatches one field-private commoncap success token with the exact
    /// actor's composite state. The complete vector is validated before the
    /// mandatory commoncap module or any stacked policy callback runs.
    pub(super) fn dispatch_capable_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreCapabilitySecurityContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            debug_assert_eq!(registered.id, actor_state.module_id);
            registered
                .module
                .capable(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_prepared_credential_capable(
        &self,
        source: &Cred,
        proposed: &Cred,
        context: &CorePreparedCredentialCapabilityContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(source.core(), context.source_credential())
            || !core::ptr::eq(proposed.core(), context.proposed_credential())
        {
            return Err(AxError::OperationNotPermitted);
        }
        let source_slots = self.credential_slots(source)?;
        let proposed_slots = self.security_slots(proposed.security())?;
        for ((registered, source_state), proposed_state) in
            self.modules.iter().zip(source_slots).zip(proposed_slots)
        {
            if registered.id != source_state.module_id || registered.id != proposed_state.module_id
            {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.prepared_credential_capable(
                context,
                source_state.erased.as_ref(),
                proposed_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    /// Dispatches an already-preflighted, immutable pair in registry order.
    /// Publication cannot be rolled back, so this pass is intentionally
    /// infallible and has no allocation or short-circuit path.
    pub(super) fn notify_credential_committed(
        &self,
        old: &Cred,
        new: &Cred,
        transition: CredentialStateTransition,
    ) {
        let old_slots = &old.security().slots;
        let new_slots = &new.security().slots;
        debug_assert_eq!(old_slots.len(), self.modules.len());
        debug_assert_eq!(new_slots.len(), self.modules.len());
        for ((registered, old_state), new_state) in
            self.modules.iter().zip(old_slots).zip(new_slots)
        {
            debug_assert_eq!(registered.id, old_state.module_id);
            debug_assert_eq!(registered.id, new_state.module_id);
            registered.module.credential_committed(
                old.core(),
                old_state.erased.as_ref(),
                new.core(),
                new_state.erased.as_ref(),
                transition,
            );
        }
    }

    /// Delivers one already-visible child credential lifecycle event in frozen
    /// order. `PendingCredentialPublication` completed every fallible registry,
    /// layout, and erased-runtime check before TASK_TABLE publication.
    pub(super) fn notify_credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source: &Cred,
        published: &Cred,
    ) {
        debug_assert!(core::ptr::eq(context.source_credential(), source.core()));
        debug_assert!(core::ptr::eq(
            context.published_credential(),
            published.core()
        ));
        let source_slots = &source.security().slots;
        let published_slots = &published.security().slots;
        debug_assert_eq!(source_slots.len(), self.modules.len());
        debug_assert_eq!(published_slots.len(), self.modules.len());
        for ((registered, source_state), published_state) in
            self.modules.iter().zip(source_slots).zip(published_slots)
        {
            debug_assert_eq!(registered.id, source_state.module_id);
            debug_assert_eq!(registered.id, published_state.module_id);
            registered.module.credential_published(
                context,
                source_state.erased.as_ref(),
                published_state.erased.as_ref(),
            );
        }
    }

    pub(super) fn try_empty_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
        derivation: CredentialStateDerivation,
    ) -> AxResult<CredentialSecurityState> {
        self.try_empty_credential_state_with_reservation(registry, self.modules.len(), derivation)
    }

    pub(super) fn try_empty_credential_state_with_reservation(
        &'static self,
        registry: FrozenSecurityRegistry,
        reservation: usize,
        derivation: CredentialStateDerivation,
    ) -> AxResult<CredentialSecurityState> {
        if !core::ptr::eq(self, registry.registry()) {
            return Err(AxError::OperationNotPermitted);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(reservation)
            .map_err(|_| AxError::NoMemory)?;
        let initial = matches!(&derivation, CredentialStateDerivation::Initial);
        let identity = Arc::try_new(CredentialStateIdentity {
            publication_claimed: AtomicBool::new(initial),
            live: AtomicBool::new(initial),
        })
        .map_err(|_| AxError::NoMemory)?;
        Ok(CredentialSecurityState {
            registry,
            slots,
            identity,
            derivation,
        })
    }

    pub(super) fn try_init_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
        credential: &CoreCred,
    ) -> AxResult<CredentialSecurityState> {
        let mut candidate =
            self.try_empty_credential_state(registry, CredentialStateDerivation::Initial)?;
        for registered in &self.modules {
            let erased = registered.module.clone().try_init_credential(credential)?;
            candidate.slots.push(OwnedModuleCredState {
                module_id: registered.id,
                erased,
            });
        }
        candidate.validate_for(registry)?;
        self.validate_erased_slots(&candidate.slots)?;
        Ok(candidate)
    }

    pub(super) fn try_prepare_credential_state(
        &'static self,
        registry: FrozenSecurityRegistry,
        old_credential: &CoreCred,
        old_state: &CredentialSecurityState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<CredentialSecurityState> {
        old_state.validate_for(registry)?;
        old_state.validate_live()?;
        self.validate_erased_slots(&old_state.slots)?;
        let mut candidate = self.try_empty_credential_state(
            registry,
            CredentialStateDerivation::Prepared {
                source: old_state.identity.clone(),
                transition,
            },
        )?;
        for (registered, old_slot) in self.modules.iter().zip(&old_state.slots) {
            if registered.id != old_slot.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            let erased = registered.module.clone().try_prepare_credential(
                old_credential,
                old_slot.erased.as_ref(),
                proposed_credential,
                transition,
            )?;
            candidate.slots.push(OwnedModuleCredState {
                module_id: registered.id,
                erased,
            });
        }
        candidate.validate_for(registry)?;
        self.validate_erased_slots(&candidate.slots)?;

        // Authorization is a separate, allocation-free pass over a complete
        // proposal. First denial aborts and reverse-drops the whole candidate.
        for ((registered, old_slot), proposed_slot) in self
            .modules
            .iter()
            .zip(&old_state.slots)
            .zip(&candidate.slots)
        {
            registered.module.authorize_credential(
                old_credential,
                old_slot.erased.as_ref(),
                proposed_credential,
                proposed_slot.erased.as_ref(),
                transition,
            )?;
        }
        Ok(candidate)
    }

    pub(super) fn dispatch_inode_permission(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_permission(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_permission_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_xattr(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_xattr(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        // Validate the complete immutable actor vector before the first hook.
        // The successful pass is the proof retained by the linear admission.
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_xattr_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn notify_inode_post_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_post_xattr(context);
        }
    }

    pub(super) fn notify_inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
    ) {
        let actor = &context.actor().security().slots;
        debug_assert!(core::ptr::eq(
            self,
            context.actor().security().registry().registry()
        ));
        debug_assert_eq!(actor.len(), self.modules.len());
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            debug_assert_eq!(registered.id, actor_state.module_id);
            registered
                .module
                .inode_post_xattr_with_credential_state(context, actor_state.erased.as_ref());
        }
    }

    pub(super) fn dispatch_inode_setattr(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_setattr(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        // Validate the complete immutable actor vector before the first module
        // runs. A successful return is the preflight proof carried into the
        // infallible post-publication pass.
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_setattr_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn notify_inode_post_setattr(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
    ) {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_post_setattr(context);
        }
    }

    pub(super) fn notify_inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
    ) {
        let actor = &context.actor().security().slots;
        debug_assert!(core::ptr::eq(
            self,
            context.actor().security().registry().registry()
        ));
        debug_assert_eq!(actor.len(), self.modules.len());
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            debug_assert_eq!(registered.id, actor_state.module_id);
            registered
                .module
                .inode_post_setattr_with_credential_state(context, actor_state.erased.as_ref());
        }
    }

    pub(super) fn dispatch_inode_create(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_create(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_create_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_mkdir(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_mkdir(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_mkdir_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_mknod(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_mknod(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_mknod_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_symlink(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_symlink(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_symlink_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_link(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_link(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_link_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_unlink(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_unlink(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_unlink_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_rmdir(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_rmdir(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_rmdir_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_rename(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.inode_rename(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .inode_rename_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_file_open(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.file_open(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .file_open_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
    ) -> AxResult<()> {
        // Validate the complete vector before the first callback so a malformed
        // late slot cannot produce a partial policy trace.
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .socket_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_mmap_file_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreMmapFileContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .mmap_file_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_mmap_addr_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreMmapAddressContext<'_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .mmap_addr_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_file_mprotect_with_credential_state(
        &self,
        actor: &Cred,
        context: &CoreFileMprotectContext<'_, '_>,
    ) -> AxResult<()> {
        if !core::ptr::eq(actor.core(), context.actor()) {
            return Err(AxError::OperationNotPermitted);
        }
        let actor_slots = self.credential_slots(actor)?;
        for (registered, actor_state) in self.modules.iter().zip(actor_slots) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .file_mprotect_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.ptrace_access(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        let target = self.credential_slots(context.target())?;
        for ((registered, actor_state), target_state) in self.modules.iter().zip(actor).zip(target)
        {
            if registered.id != actor_state.module_id || registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.ptrace_access_with_credential_state(
                context,
                actor_state.erased.as_ref(),
                target_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    pub(super) fn dispatch_ptrace_traceme(
        &self,
        context: &PtraceTracemeContext<'_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.ptrace_traceme(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
    ) -> AxResult<()> {
        let parent = self.credential_slots(context.parent_actor())?;
        let child = self.credential_slots(context.child_target())?;
        for ((registered, parent_state), child_state) in self.modules.iter().zip(parent).zip(child)
        {
            if registered.id != parent_state.module_id || registered.id != child_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.ptrace_traceme_with_credential_state(
                context,
                parent_state.erased.as_ref(),
                child_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    pub(super) fn dispatch_exec_credential(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.exec_credential(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
    ) -> AxResult<()> {
        let old = self.credential_slots(context.old())?;
        let proposed = self.security_slots(context.draft().proposed_security())?;
        for ((registered, old_state), proposed_state) in self.modules.iter().zip(old).zip(proposed)
        {
            if registered.id != old_state.module_id || registered.id != proposed_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.exec_credential_with_credential_state(
                context,
                old_state.erased.as_ref(),
                proposed_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    pub(super) fn dispatch_exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        for (registered, actor_state) in self.modules.iter().zip(actor) {
            if registered.id != actor_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .exec_executable_with_credential_state(context, actor_state.erased.as_ref())?;
        }
        Ok(())
    }

    /// Runs a preflighted infallible phase over exact old/new module states.
    /// `PendingExecSecurity::try_new` has already validated every erased slot,
    /// so no failure branch can appear after exec crosses its point of no
    /// return.
    pub(super) fn notify_exec_committing(&self, context: &ExecCommittingSecurityContext<'_>) {
        let old = &context.old().security().slots;
        let new = &context.new_credential().security().slots;
        debug_assert_eq!(old.len(), self.modules.len());
        debug_assert_eq!(new.len(), self.modules.len());
        for ((registered, old_state), new_state) in self.modules.iter().zip(old).zip(new) {
            debug_assert_eq!(registered.id, old_state.module_id);
            debug_assert_eq!(registered.id, new_state.module_id);
            registered.module.exec_committing(
                context,
                old_state.erased.as_ref(),
                new_state.erased.as_ref(),
            );
        }
    }

    pub(super) fn notify_exec_committed(&self, context: &ExecCommittedSecurityContext<'_>) {
        let old = &context.old().security().slots;
        let new = &context.new_credential().security().slots;
        debug_assert_eq!(old.len(), self.modules.len());
        debug_assert_eq!(new.len(), self.modules.len());
        for ((registered, old_state), new_state) in self.modules.iter().zip(old).zip(new) {
            debug_assert_eq!(registered.id, old_state.module_id);
            debug_assert_eq!(registered.id, new_state.module_id);
            registered.module.exec_committed(
                context,
                old_state.erased.as_ref(),
                new_state.erased.as_ref(),
            );
        }
    }

    pub(super) fn dispatch_scheduler(
        &self,
        context: &SecuritySchedulerContext<'_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.scheduler(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        let target = self.credential_slots(context.target())?;
        for ((registered, actor_state), target_state) in self.modules.iter().zip(actor).zip(target)
        {
            if registered.id != actor_state.module_id || registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.scheduler_with_credential_state(
                context,
                actor_state.erased.as_ref(),
                target_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    pub(super) fn dispatch_task_getsid(
        &self,
        context: &SecurityTaskGetsidContext<'_>,
    ) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.task_getsid(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_task_getsid_with_credential_state(
        &self,
        context: &SecurityTaskGetsidContext<'_>,
    ) -> AxResult<()> {
        let target = self.credential_slots(context.target())?;
        for (registered, target_state) in self.modules.iter().zip(target) {
            if registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .task_getsid_with_credential_state(context, target_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_task_getscheduler_with_credential_state(
        &self,
        context: &SecurityTaskGetSchedulerContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        let target = self.credential_slots(context.target())?;
        for ((registered, actor_state), target_state) in self.modules.iter().zip(actor).zip(target)
        {
            if registered.id != actor_state.module_id || registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.task_getscheduler_with_credential_state(
                context,
                actor_state.erased.as_ref(),
                target_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }

    pub(super) fn dispatch_task_getpgid_with_credential_state(
        &self,
        context: &SecurityTaskGetpgidContext<'_>,
    ) -> AxResult<()> {
        let target = self.credential_slots(context.target())?;
        for (registered, target_state) in self.modules.iter().zip(target) {
            if registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered
                .module
                .task_getpgid_with_credential_state(context, target_state.erased.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn dispatch_signal(&self, context: &SecuritySignalContext<'_>) -> AxResult<()> {
        for (index, registered) in self.modules.iter().enumerate() {
            debug_assert_eq!(usize::from(registered.id.0), index);
            registered.module.signal(context)?;
        }
        Ok(())
    }

    pub(super) fn dispatch_signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
    ) -> AxResult<()> {
        let actor = self.credential_slots(context.actor())?;
        let target = self.credential_slots(context.target())?;
        for ((registered, actor_state), target_state) in self.modules.iter().zip(actor).zip(target)
        {
            if registered.id != actor_state.module_id || registered.id != target_state.module_id {
                return Err(AxError::OperationNotPermitted);
            }
            registered.module.signal_with_credential_state(
                context,
                actor_state.erased.as_ref(),
                target_state.erased.as_ref(),
            )?;
        }
        Ok(())
    }
}

impl FrozenSecurityRegistry {
    pub(in crate::task) fn try_init_credential_state(
        self,
        credential: &CoreCred,
    ) -> AxResult<CredentialSecurityState> {
        self.registry().try_init_credential_state(self, credential)
    }

    pub(in crate::task) fn try_prepare_credential_state(
        self,
        old_credential: &CoreCred,
        old_state: &CredentialSecurityState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<CredentialSecurityState> {
        self.registry().try_prepare_credential_state(
            self,
            old_credential,
            old_state,
            proposed_credential,
            transition,
        )
    }
}

impl Drop for SecurityRegistry {
    fn drop(&mut self) {
        while self.modules.pop().is_some() {}
    }
}

pub(crate) struct SecurityRegistryPublication {
    pub(super) registry: Once<SecurityRegistry>,
}

impl SecurityRegistryPublication {
    pub(super) const fn new() -> Self {
        Self {
            registry: Once::new(),
        }
    }

    /// Serializes construction as well as publication. `spin::Once` retries
    /// after a failed initializer and never invokes a losing caller's closure
    /// after another caller succeeds. The local flag distinguishes that first
    /// success from a later call that merely observed the published value.
    pub(super) fn try_publish_with<F>(
        &self,
        build: F,
    ) -> Result<&SecurityRegistry, RegistryBuildError>
    where
        F: FnOnce() -> Result<SecurityRegistry, RegistryBuildError>,
    {
        let mut initialized_here = false;
        let registry = self.registry.try_call_once(|| {
            initialized_here = true;
            build()
        })?;
        if initialized_here {
            Ok(registry)
        } else {
            Err(RegistryBuildError::AlreadyPublished)
        }
    }

    #[cfg(test)]
    pub(super) fn get(&self) -> Option<&SecurityRegistry> {
        self.registry.get()
    }
}

pub(crate) static SECURITY_REGISTRY: SecurityRegistryPublication =
    SecurityRegistryPublication::new();

#[cfg(test)]
pub(crate) static TEST_SECURITY_REGISTRY: Once<SecurityRegistry> = Once::new();

pub(crate) fn try_build_builtin_registry() -> Result<SecurityRegistry, RegistryBuildError> {
    let mut builder = SecurityRegistryBuilder::try_new()?.try_register_commoncap()?;
    builder.try_register::<NoopPolicyModule>()?;
    Ok(builder.freeze())
}

/// Builds, freezes, and publishes the complete registry before userspace.
pub(crate) fn init() -> Result<FrozenSecurityRegistry, RegistryBuildError> {
    let registry = SECURITY_REGISTRY.try_publish_with(try_build_builtin_registry)?;
    Ok(FrozenSecurityRegistry(registry))
}

#[cfg(test)]
pub(in crate::task) fn test_frozen_registry() -> FrozenSecurityRegistry {
    FrozenSecurityRegistry(
        TEST_SECURITY_REGISTRY
            .call_once(|| try_build_builtin_registry().expect("failed to build test registry")),
    )
}

#[cfg(test)]
pub(crate) fn require_published_registry(
    registry: Option<&SecurityRegistry>,
) -> AxResult<&SecurityRegistry> {
    registry.ok_or(AxError::OperationNotPermitted)
}
