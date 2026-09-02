//! The security-module contract and its type-erased runtime form.
//!
//! `SecurityModule` is the surface a policy implements. `ErasedSecurityModule`
//! is what the registry stores: a blanket impl erases the module type so one
//! immutable declaration order can hold heterogeneous modules without
//! allocating per dispatch. Per-module credential state is owned here too,
//! because its lifetime is the module's rather than the registry's.

use super::*;

/// One security module owns every hook family as one registration unit.
///
/// The defaults are explicit no-ops so a module cannot be partially inserted
/// into independent per-hook registries. Boot initialization must return an
/// owned runtime object; dropping that object rolls back all module-local boot
/// resources if a later registry step fails.
pub(super) trait SecurityModule: Send + Sync + 'static {
    const KEY: ModuleKey;
    type CredentialState: Send + Sync + 'static;

    fn try_boot_init() -> Result<Self, RegistryBuildError>
    where
        Self: Sized;

    /// Constructs this module's state for the initial root credential.
    fn try_init_credential(&self, credential: &CoreCred) -> AxResult<Self::CredentialState>;

    /// Constructs a complete unpublished replacement state from the exact
    /// old composite credential. This is the only fallible state callback.
    fn try_prepare_credential(
        &self,
        old_credential: &CoreCred,
        old_state: &Self::CredentialState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<Self::CredentialState>;

    /// Authorizes one fully prepared old/new state pair. Dispatch is ordered
    /// and deny-first; a denial drops every proposed state before publication.
    fn authorize_credential(
        &self,
        _old_credential: &CoreCred,
        _old_state: &Self::CredentialState,
        _proposed_credential: &CoreCred,
        _proposed_state: &Self::CredentialState,
        _transition: CredentialStateTransition,
    ) -> AxResult<()> {
        Ok(())
    }

    /// Observes one successfully published credential replacement.
    ///
    /// This is an infallible Linux-LSM-style atomic notification. It runs in
    /// frozen registry order after every credential publication, process-
    /// security, image, group-leader, and task-alias lock has been released.
    /// The credential writer mutex is also released before entry. The method
    /// must not allocate, block, fail, panic, resample `current()`, reenter a
    /// credential update, or acquire task/process/image/alias locks. Work that
    /// can sleep must use a resource reserved before publication and perform
    /// only an allocation-free handoff here. Registry order is guaranteed
    /// within this exact transition; notifications from other slots or a
    /// later transaction may run concurrently, so module-owned aggregation
    /// must already be concurrency-safe without allocating here. This hook is
    /// for replacement of an already-live slot (`Mutation` and `Exec`) only;
    /// initial, fork-child, and user-namespace object publication require
    /// distinct lifecycle notifications and do not masquerade as a commit.
    fn credential_committed(
        &self,
        _context: CredentialPostCommitContext<'_, Self::CredentialState>,
    ) {
    }

    /// Narrows one exact commoncap-approved capability request.
    ///
    /// The external context can only be produced after validated-number and
    /// namespace/effective-set authorization. Dispatch has also validated the
    /// actor's complete composite state vector before the first module runs.
    /// Hooks are ordered, deny-first, allocation-free, and nonblocking; they
    /// must not call `current()` or reenter capability/security dispatch.
    fn capable(&self, _context: &CoreCapabilitySecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn capable_with_credential_state(
        &self,
        context: &CoreCapabilitySecurityContext<'_>,
        actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = actor_state;
        self.capable(context)
    }

    /// Linux `security_locked_down(LOCKDOWN_IOPORT)` authorization.
    ///
    /// The hook is intentionally independent of capability authorization:
    /// lockdown may deny an operation even after commoncap accepted
    /// CAP_SYS_RAWIO.
    fn locked_down_ioport(&self, _actor: &CoreCred) -> AxResult<()> {
        Ok(())
    }

    fn locked_down_ioport_with_credential_state(
        &self,
        actor: &CoreCred,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.locked_down_ioport(actor)
    }

    /// Authorizes loading a replacement kernel image after commoncap.
    /// Implementations may enforce lockdown, measured-boot, or signature
    /// policy independently for orderly and crash images.
    fn kernel_load_data(
        &self,
        _actor: &CoreCred,
        _kind: KernelLoadKind,
        _from_file: bool,
    ) -> AxResult<()> {
        Ok(())
    }

    fn kernel_load_data_with_credential_state(
        &self,
        actor: &CoreCred,
        kind: KernelLoadKind,
        from_file: bool,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.kernel_load_data(actor, kind, from_file)
    }

    /// Narrows commoncap authority derived from a fully prepared credential
    /// without treating that credential as an already-live actor.
    fn prepared_credential_capable(
        &self,
        _context: &CorePreparedCredentialCapabilityContext<'_>,
    ) -> AxResult<()> {
        Ok(())
    }

    fn prepared_credential_capable_with_state(
        &self,
        context: &CorePreparedCredentialCapabilityContext<'_>,
        source_state: &Self::CredentialState,
        proposed_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = (source_state, proposed_state);
        self.prepared_credential_capable(context)
    }

    /// Observes a separately prepared fork or user-namespace credential after
    /// its exact child target has become visible.
    ///
    /// Preparation, state authorization, and complete source/published layout
    /// validation finish before visibility. This callback is infallible and
    /// runs in frozen registry order after task-table and parent-publication
    /// locks are released. It must not allocate, block, fail, panic, resample
    /// `current()`, or reenter task, credential, or security operations.
    fn credential_published(
        &self,
        _context: &CoreCredentialPublicationContext<'_>,
        _source_state: &Self::CredentialState,
        _published_state: &Self::CredentialState,
    ) {
    }

    /// Releases one module state through the module runtime that created it.
    /// The owner wrapper invokes this directly and never consults a registry.
    ///
    /// This is a Linux-LSM-style atomic teardown callback: it must not block,
    /// allocate, reenter credential update, or acquire task/process/image
    /// locks. Rollback may call it while the sleepable credential-writer mutex
    /// is owned, but never under credential publication, image, alias, or
    /// registry spin locks. Committed retired states are freed after the writer
    /// mutex as well. A module needing sleepable cleanup must transfer an owned
    /// token to an independently preallocated deferred-work mechanism.
    fn free_credential(&self, state: Self::CredentialState) {
        drop(state);
    }

    /// Authorizes one exact inode after the caller has completed Linux DAC.
    ///
    /// Dispatch may run inside the filesystem-context and pathwalk lock
    /// domains. Implementations must remain allocation-free and nonblocking,
    /// must not call `current()`, and must not reenter VFS lookup, credential
    /// update, or security dispatch.
    fn inode_permission(&self, _context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_permission(context)
    }

    /// Authorizes one exact typed xattr operation before provider dispatch.
    ///
    /// The caller has frozen the actor, DAC identity, filesystem owner
    /// namespace, target identity, and validated borrowed operation. Hooks are
    /// allocation-free and nonblocking and must not call `current()`, reenter
    /// VFS/xattr/security dispatch, or mutate provider state.
    fn inode_xattr(&self, _context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_xattr(context)
    }

    /// Observes one successfully completed provider xattr operation.
    ///
    /// The infallible callback receives the exact context admitted by the pre
    /// hook. It is never emitted for pre-hook denial or provider failure and
    /// must not allocate, block, fail, panic, resample `current()`, or reenter
    /// VFS/xattr/security dispatch.
    fn inode_post_xattr(&self, _context: &InodeXattrSecurityContext<'_, '_>) {}

    fn inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) {
        self.inode_post_xattr(context);
    }

    /// Authorizes one exact inode-attribute proposal at the Linux
    /// `security_inode_setattr` point. The caller has frozen the actor, DAC
    /// identity, owner namespace, old inode snapshot, typed intent, and
    /// hook-point proposal, but has not yet run its fallible privilege cleanup
    /// or backend publication.
    fn inode_setattr(&self, _context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_setattr(context)
    }

    /// Observes one successfully committed inode-attribute change.
    ///
    /// This is an infallible, allocation-free post hook. It receives the same
    /// actor, DAC identity, owner namespace, and proposal admitted by the pre
    /// hook together with a caller-frozen committed inode snapshot. It must not
    /// call `current()`, reenter VFS/security dispatch, block, allocate, fail,
    /// or panic.
    fn inode_post_setattr(&self, _context: &InodePostSetattrSecurityContext<'_, '_>) {}

    fn inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) {
        self.inode_post_setattr(context);
    }

    /// Authorizes one exact planned named entry after the caller has found the
    /// destination absent and before a regular file is created. The parent
    /// snapshot, final component, actor, DAC identity, owner namespace, and
    /// final normalized mode are all frozen by the caller. Implementations
    /// must remain allocation-free and nonblocking and must not call
    /// `current()` or reenter VFS, credential, or security operations.
    fn inode_create(&self, _context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_create(context)
    }

    /// Authorizes one exact planned named entry before a directory is created,
    /// under the same frozen, lookup-free contract as [`Self::inode_create`].
    fn inode_mkdir(&self, _context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_mkdir(context)
    }

    /// Authorizes one exact planned named entry before a FIFO, device, or
    /// socket inode is created, with an already-validated kind/mode/rdev
    /// operation and no VFS handle exposed to policy.
    fn inode_mknod(&self, _context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_mknod(context)
    }

    /// Authorizes one exact planned symbolic link after destination DAC and
    /// absence revalidation, before the target or name is published. The
    /// caller freezes the exact target which the filesystem will store.
    fn inode_symlink(&self, _context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_symlink(context)
    }

    /// Authorizes one exact hard-link source and prospective destination after
    /// source eligibility, destination DAC, cross-filesystem rejection, and
    /// absence revalidation. The caller must publish a new name for that same
    /// frozen source. Hooks are allocation-free, nonblocking, and forbidden
    /// from VFS/current/credential/security reentry.
    fn inode_link(&self, _context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_link(context)
    }

    /// Authorizes removal of one exact existing non-directory entry after the
    /// caller has completed its may-delete-style admission and revalidation.
    /// The frozen entry binds the exact parent, victim, and final name which the
    /// transaction must remove. Hooks must not reenter VFS or resample state.
    fn inode_unlink(&self, _context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_unlink(context)
    }

    /// Authorizes removal of one exact existing directory entry. Directory
    /// removal has its own hook contract and is never selected by a boolean on
    /// [`Self::inode_unlink`].
    fn inode_rmdir(&self, _context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_rmdir(context)
    }

    /// Authorizes one exact rename leaf after the caller has frozen the old
    /// parent/source entry and new parent/destination entry. The destination
    /// entry explicitly preserves absent versus existing-target state. Linux
    /// rename flags and exchange's ordered reverse dispatch remain caller
    /// responsibilities and are not represented by this leaf contract.
    fn inode_rename(&self, _context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.inode_rename(context)
    }

    /// Authorizes one exact, fully resolved location before the open
    /// transaction publishes an fd, persistent executable-write reservation,
    /// filesystem-open side effect, fanotify open permission, POSIX lease
    /// conflict handling, or truncate. Dispatch is deny-first and stops on the
    /// first error. Implementations must not allocate, block, call `current()`,
    /// or reenter VFS, open, credential, or security operations.
    fn file_open(&self, _context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.file_open(context)
    }

    /// Authorizes one already prepared socket leaf. Every variant borrows only
    /// immutable kernel-owned facts and dispatch runs before backend mutation.
    fn socket(&self, _context: &SocketSecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.socket(context)
    }

    /// Authorizes one normalized file/anonymous mmap request after local ABI
    /// and backend validation, before address policy or backend construction.
    fn mmap_file(&self, _context: &CoreMmapFileContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn mmap_file_with_credential_state(
        &self,
        context: &CoreMmapFileContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.mmap_file(context)
    }

    /// Authorizes the final selected address in one exact retained image.
    fn mmap_addr(&self, _context: &CoreMmapAddressContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn mmap_addr_with_credential_state(
        &self,
        context: &CoreMmapAddressContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.mmap_addr(context)
    }

    /// Authorizes one exact pre-change VMA segment while the prepared
    /// protection transaction can still be dropped without side effects.
    fn file_mprotect(&self, _context: &CoreFileMprotectContext<'_, '_>) -> AxResult<()> {
        Ok(())
    }

    fn file_mprotect_with_credential_state(
        &self,
        context: &CoreFileMprotectContext<'_, '_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.file_mprotect(context)
    }

    fn ptrace_access(&self, _context: &PtraceAccessContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
        _actor_state: &Self::CredentialState,
        _target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.ptrace_access(context)
    }

    fn ptrace_traceme(&self, _context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
        _parent_state: &Self::CredentialState,
        _child_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.ptrace_traceme(context)
    }

    fn exec_credential(&self, _context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
        _old_state: &Self::CredentialState,
        _proposed_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.exec_credential(context)
    }

    /// Authorizes one executable component after DAC/open admission and before
    /// its binary handler or mapping is used. This fallible hook is invoked for
    /// the requested object, every shebang interpreter, and `PT_INTERP`.
    ///
    /// Dispatch currently runs inside the global ELF-loader cache mutex. A hook
    /// must not block, allocate, resample task state, or reenter loader/VFS
    /// operations. Any fallible policy state must be reserved before dispatch.
    fn exec_executable(&self, _context: &ExecExecutableSecurityContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
        _actor_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.exec_executable(context)
    }

    /// Observes the exec point of no return after CLOEXEC/AIO cleanup and
    /// immediately before composite credential/image publication. This hook is
    /// infallible and allocation-free. The credential writer mutex is still
    /// held, but no publication/process/image/alias spin lock is held.
    /// Implementations must not block, allocate, resample task state, or
    /// reenter credential/VFS/loader operations.
    fn exec_committing(
        &self,
        _context: &ExecCommittingSecurityContext<'_>,
        _old_state: &Self::CredentialState,
        _new_state: &Self::CredentialState,
    ) {
    }

    /// Observes the complete installed exec image after the generic credential
    /// notification, hardware page-table-root switch, executable/metadata and
    /// userspace-context installation, ptrace exec stop, and release of every
    /// writer/publication/process/image/alias/action lock. It must remain
    /// infallible and allocation-free and must not resample or reenter task,
    /// credential, VFS, or loader operations.
    fn exec_committed(
        &self,
        _context: &ExecCommittedSecurityContext<'_>,
        _old_state: &Self::CredentialState,
        _new_state: &Self::CredentialState,
    ) {
    }

    fn scheduler(&self, _context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
        actor_state: &Self::CredentialState,
        target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = (actor_state, target_state);
        self.scheduler(context)
    }

    /// Linux `security_task_getsid` authorization for one resolved task.
    fn task_getsid(&self, _context: &SecurityTaskGetsidContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn task_getsid_with_credential_state(
        &self,
        context: &SecurityTaskGetsidContext<'_>,
        _target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.task_getsid(context)
    }

    /// Linux `security_task_getscheduler` authorization for one resolved
    /// actor/target pair.
    fn task_getscheduler(&self, _context: &SecurityTaskGetSchedulerContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn task_getscheduler_with_credential_state(
        &self,
        context: &SecurityTaskGetSchedulerContext<'_>,
        _actor_state: &Self::CredentialState,
        _target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.task_getscheduler(context)
    }

    /// Linux `security_task_getpgid` authorization for one resolved task.
    fn task_getpgid(&self, _context: &SecurityTaskGetpgidContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn task_getpgid_with_credential_state(
        &self,
        context: &SecurityTaskGetpgidContext<'_>,
        _target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        self.task_getpgid(context)
    }

    fn signal(&self, _context: &SecuritySignalContext<'_>) -> AxResult<()> {
        Ok(())
    }

    fn signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
        actor_state: &Self::CredentialState,
        target_state: &Self::CredentialState,
    ) -> AxResult<()> {
        let _ = (actor_state, target_state);
        self.signal(context)
    }
}

/// Object-safe runtime view of a source-facing module. The adapter keeps the
/// compile-time key and fallible initializer out of dispatch's trait object.
pub(super) trait ErasedSecurityModule: Send + Sync {
    fn validate_credential_state(&self, state: &dyn ErasedOwnedCredentialState) -> AxResult<()>;
    fn try_init_credential(
        self: Arc<Self>,
        credential: &CoreCred,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>>;
    fn try_prepare_credential(
        self: Arc<Self>,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>>;
    fn authorize_credential(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        proposed_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    ) -> AxResult<()>;
    fn credential_committed(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        new_credential: &CoreCred,
        new_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    );
    fn capable(
        &self,
        context: &CoreCapabilitySecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn locked_down_ioport(
        &self,
        actor: &CoreCred,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn kernel_load_data(
        &self,
        actor: &CoreCred,
        kind: KernelLoadKind,
        from_file: bool,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn prepared_credential_capable(
        &self,
        context: &CorePreparedCredentialCapabilityContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        published_state: &dyn ErasedOwnedCredentialState,
    );
    fn inode_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()>;
    fn inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()>;
    fn inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_post_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>);
    fn inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    );
    fn inode_setattr(&self, context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()>;
    fn inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_post_setattr(&self, context: &InodePostSetattrSecurityContext<'_, '_>);
    fn inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    );
    fn inode_create(&self, context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_mkdir(&self, context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_mknod(&self, context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_symlink(&self, context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_unlink(&self, context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_rmdir(&self, context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()>;
    fn inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn inode_rename(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()>;
    fn inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn file_open(&self, context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()>;
    fn file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn mmap_file_with_credential_state(
        &self,
        context: &CoreMmapFileContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn mmap_addr_with_credential_state(
        &self,
        context: &CoreMmapAddressContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn file_mprotect_with_credential_state(
        &self,
        context: &CoreFileMprotectContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()>;
    fn ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()>;
    fn ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
        parent_state: &dyn ErasedOwnedCredentialState,
        child_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()>;
    fn exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn exec_committing(
        &self,
        context: &ExecCommittingSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    );
    fn exec_committed(
        &self,
        context: &ExecCommittedSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    );
    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()>;
    fn scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn task_getsid(&self, context: &SecurityTaskGetsidContext<'_>) -> AxResult<()>;
    fn task_getsid_with_credential_state(
        &self,
        context: &SecurityTaskGetsidContext<'_>,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn task_getscheduler(&self, context: &SecurityTaskGetSchedulerContext<'_>) -> AxResult<()>;
    fn task_getscheduler_with_credential_state(
        &self,
        context: &SecurityTaskGetSchedulerContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;

    fn task_getpgid(&self, context: &SecurityTaskGetpgidContext<'_>) -> AxResult<()>;
    fn task_getpgid_with_credential_state(
        &self,
        context: &SecurityTaskGetpgidContext<'_>,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
    fn signal(&self, context: &SecuritySignalContext<'_>) -> AxResult<()>;
    fn signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()>;
}

pub(crate) trait ErasedOwnedCredentialState: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

/// Keeps the creating runtime alive until its last credential state is freed.
/// Its explicit `Drop` callback means teardown never has to look up a module
/// by ID in global or registry-owned storage.
pub(super) struct OwnedCredentialState<M: SecurityModule> {
    pub(super) module: Arc<M>,
    pub(super) state: Option<M::CredentialState>,
}

impl<M: SecurityModule> OwnedCredentialState<M> {
    pub(super) fn state(&self) -> &M::CredentialState {
        self.state
            .as_ref()
            .expect("owned credential state was already released")
    }
}

impl<M: SecurityModule> ErasedOwnedCredentialState for OwnedCredentialState<M> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<M: SecurityModule> Drop for OwnedCredentialState<M> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            self.module.free_credential(state);
        }
    }
}

pub(super) fn try_own_credential_state<M: SecurityModule>(
    module: Arc<M>,
    state: M::CredentialState,
) -> AxResult<Box<dyn ErasedOwnedCredentialState>> {
    try_own_credential_state_with(module, state, |state| {
        Box::try_new(state).map_err(|_| AxError::NoMemory)
    })
}

pub(super) fn try_own_credential_state_with<M, F>(
    module: Arc<M>,
    state: M::CredentialState,
    allocate: F,
) -> AxResult<Box<dyn ErasedOwnedCredentialState>>
where
    M: SecurityModule,
    F: FnOnce(OwnedCredentialState<M>) -> AxResult<Box<OwnedCredentialState<M>>>,
{
    let state = allocate(OwnedCredentialState {
        module,
        state: Some(state),
    })?;
    Ok(state)
}

pub(super) fn owned_credential_state<'a, M: SecurityModule>(
    module: &M,
    state: &'a dyn ErasedOwnedCredentialState,
) -> AxResult<&'a M::CredentialState> {
    let state = state
        .as_any()
        .downcast_ref::<OwnedCredentialState<M>>()
        .ok_or(AxError::OperationNotPermitted)?;
    if !core::ptr::eq(module, Arc::as_ptr(&state.module)) {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(state.state())
}

impl<M: SecurityModule> ErasedSecurityModule for M {
    fn validate_credential_state(&self, state: &dyn ErasedOwnedCredentialState) -> AxResult<()> {
        owned_credential_state(self, state).map(|_| ())
    }

    fn try_init_credential(
        self: Arc<Self>,
        credential: &CoreCred,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>> {
        let state = SecurityModule::try_init_credential(self.as_ref(), credential)?;
        try_own_credential_state(self, state)
    }

    fn try_prepare_credential(
        self: Arc<Self>,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        transition: CredentialStateTransition,
    ) -> AxResult<Box<dyn ErasedOwnedCredentialState>> {
        let old_state = owned_credential_state(self.as_ref(), old_state)?;
        let proposed_state = SecurityModule::try_prepare_credential(
            self.as_ref(),
            old_credential,
            old_state,
            proposed_credential,
            transition,
        )?;
        try_own_credential_state(self, proposed_state)
    }

    fn authorize_credential(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_credential: &CoreCred,
        proposed_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    ) -> AxResult<()> {
        SecurityModule::authorize_credential(
            self,
            old_credential,
            owned_credential_state(self, old_state)?,
            proposed_credential,
            owned_credential_state(self, proposed_state)?,
            transition,
        )
    }

    fn credential_committed(
        &self,
        old_credential: &CoreCred,
        old_state: &dyn ErasedOwnedCredentialState,
        new_credential: &CoreCred,
        new_state: &dyn ErasedOwnedCredentialState,
        transition: CredentialStateTransition,
    ) {
        // A PendingCredentialPostCommit is created only after a complete
        // release-build preflight of both immutable state vectors. Repeating
        // a fallible downcast after publication would introduce a failure
        // branch at a point where rollback is impossible, so this adapter may
        // rely on that linear token's invariant.
        let old_state = owned_credential_state(self, old_state)
            .expect("preflighted old credential state changed before notification");
        let new_state = owned_credential_state(self, new_state)
            .expect("preflighted new credential state changed before notification");
        SecurityModule::credential_committed(
            self,
            CredentialPostCommitContext {
                old_credential,
                old_state,
                new_credential,
                new_state,
                transition,
            },
        );
    }

    fn capable(
        &self,
        context: &CoreCapabilitySecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::capable_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn locked_down_ioport(
        &self,
        actor: &CoreCred,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::locked_down_ioport_with_credential_state(
            self,
            actor,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn kernel_load_data(
        &self,
        actor: &CoreCred,
        kind: KernelLoadKind,
        from_file: bool,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::kernel_load_data_with_credential_state(
            self,
            actor,
            kind,
            from_file,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn prepared_credential_capable(
        &self,
        context: &CorePreparedCredentialCapabilityContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::prepared_credential_capable_with_state(
            self,
            context,
            owned_credential_state(self, source_state)?,
            owned_credential_state(self, proposed_state)?,
        )
    }

    fn credential_published(
        &self,
        context: &CoreCredentialPublicationContext<'_>,
        source_state: &dyn ErasedOwnedCredentialState,
        published_state: &dyn ErasedOwnedCredentialState,
    ) {
        let source_state = owned_credential_state(self, source_state)
            .expect("preflighted source credential state changed before publication callback");
        let published_state = owned_credential_state(self, published_state)
            .expect("preflighted child credential state changed before publication callback");
        SecurityModule::credential_published(self, context, source_state, published_state);
    }

    fn inode_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::inode_permission(self, context)
    }

    fn inode_permission_with_credential_state(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_permission_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::inode_xattr(self, context)
    }

    fn inode_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_xattr_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_post_xattr(&self, context: &InodeXattrSecurityContext<'_, '_>) {
        SecurityModule::inode_post_xattr(self, context);
    }

    fn inode_post_xattr_with_credential_state(
        &self,
        context: &InodeXattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) {
        // The successful pre-hook pass validates every actor state before the
        // caller can receive an admission. Provider success cannot be rolled
        // back, so the matching post pass has no fallible downcast branch.
        let actor_state = owned_credential_state(self, actor_state)
            .expect("preflighted xattr actor state changed before post notification");
        SecurityModule::inode_post_xattr_with_credential_state(self, context, actor_state);
    }

    fn inode_setattr(&self, context: &InodeSetattrSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::inode_setattr(self, context)
    }

    fn inode_setattr_with_credential_state(
        &self,
        context: &InodeSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_setattr_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_post_setattr(&self, context: &InodePostSetattrSecurityContext<'_, '_>) {
        SecurityModule::inode_post_setattr(self, context);
    }

    fn inode_post_setattr_with_credential_state(
        &self,
        context: &InodePostSetattrSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) {
        // The fallible pre-hook pass validates the complete actor state vector
        // before it returns an admission token. Publication cannot be rolled
        // back, so the matching post pass has no downcast failure branch.
        let actor_state = owned_credential_state(self, actor_state)
            .expect("preflighted setattr actor state changed before post notification");
        SecurityModule::inode_post_setattr_with_credential_state(self, context, actor_state);
    }

    fn inode_create(&self, context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_create(self, context)
    }

    fn inode_create_with_credential_state(
        &self,
        context: &InodeCreateSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_create_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_mkdir(&self, context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_mkdir(self, context)
    }

    fn inode_mkdir_with_credential_state(
        &self,
        context: &InodeMkdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_mkdir_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_mknod(&self, context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_mknod(self, context)
    }

    fn inode_mknod_with_credential_state(
        &self,
        context: &InodeMknodSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_mknod_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_symlink(&self, context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_symlink(self, context)
    }

    fn inode_symlink_with_credential_state(
        &self,
        context: &InodeSymlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_symlink_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_link(self, context)
    }

    fn inode_link_with_credential_state(
        &self,
        context: &InodeLinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_link_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_unlink(&self, context: &InodeUnlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_unlink(self, context)
    }

    fn inode_unlink_with_credential_state(
        &self,
        context: &InodeUnlinkSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_unlink_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_rmdir(&self, context: &InodeRmdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_rmdir(self, context)
    }

    fn inode_rmdir_with_credential_state(
        &self,
        context: &InodeRmdirSecurityContext<'_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_rmdir_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn inode_rename(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()> {
        SecurityModule::inode_rename(self, context)
    }

    fn inode_rename_with_credential_state(
        &self,
        context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::inode_rename_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn file_open(&self, context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
        SecurityModule::file_open(self, context)
    }

    fn file_open_with_credential_state(
        &self,
        context: &FileOpenSecurityContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::file_open_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn socket_with_credential_state(
        &self,
        context: &SocketSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::socket_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn mmap_file_with_credential_state(
        &self,
        context: &CoreMmapFileContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::mmap_file_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn mmap_addr_with_credential_state(
        &self,
        context: &CoreMmapAddressContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::mmap_addr_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn file_mprotect_with_credential_state(
        &self,
        context: &CoreFileMprotectContext<'_, '_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::file_mprotect_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn ptrace_access(&self, context: &PtraceAccessContext<'_>) -> AxResult<()> {
        SecurityModule::ptrace_access(self, context)
    }

    fn ptrace_access_with_credential_state(
        &self,
        context: &PtraceAccessContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::ptrace_access_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
            owned_credential_state(self, target_state)?,
        )
    }

    fn ptrace_traceme(&self, context: &PtraceTracemeContext<'_>) -> AxResult<()> {
        SecurityModule::ptrace_traceme(self, context)
    }

    fn ptrace_traceme_with_credential_state(
        &self,
        context: &PtraceTracemeContext<'_>,
        parent_state: &dyn ErasedOwnedCredentialState,
        child_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::ptrace_traceme_with_credential_state(
            self,
            context,
            owned_credential_state(self, parent_state)?,
            owned_credential_state(self, child_state)?,
        )
    }

    fn exec_credential(&self, context: &ExecCredentialSecurityContext<'_>) -> AxResult<()> {
        SecurityModule::exec_credential(self, context)
    }

    fn exec_credential_with_credential_state(
        &self,
        context: &ExecCredentialSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        proposed_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::exec_credential_with_credential_state(
            self,
            context,
            owned_credential_state(self, old_state)?,
            owned_credential_state(self, proposed_state)?,
        )
    }

    fn exec_executable_with_credential_state(
        &self,
        context: &ExecExecutableSecurityContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::exec_executable_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
        )
    }

    fn exec_committing(
        &self,
        context: &ExecCommittingSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    ) {
        let old_state = owned_credential_state(self, old_state)
            .expect("preflighted old exec credential state changed before committing");
        let new_state = owned_credential_state(self, new_state)
            .expect("preflighted new exec credential state changed before committing");
        SecurityModule::exec_committing(self, context, old_state, new_state);
    }

    fn exec_committed(
        &self,
        context: &ExecCommittedSecurityContext<'_>,
        old_state: &dyn ErasedOwnedCredentialState,
        new_state: &dyn ErasedOwnedCredentialState,
    ) {
        let old_state = owned_credential_state(self, old_state)
            .expect("preflighted old exec credential state changed before committed callback");
        let new_state = owned_credential_state(self, new_state)
            .expect("preflighted new exec credential state changed before committed callback");
        SecurityModule::exec_committed(self, context, old_state, new_state);
    }

    fn scheduler(&self, context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
        SecurityModule::scheduler(self, context)
    }

    fn scheduler_with_credential_state(
        &self,
        context: &SecuritySchedulerContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::scheduler_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
            owned_credential_state(self, target_state)?,
        )
    }

    fn task_getsid(&self, context: &SecurityTaskGetsidContext<'_>) -> AxResult<()> {
        SecurityModule::task_getsid(self, context)
    }

    fn task_getsid_with_credential_state(
        &self,
        context: &SecurityTaskGetsidContext<'_>,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::task_getsid_with_credential_state(
            self,
            context,
            owned_credential_state(self, target_state)?,
        )
    }

    fn task_getscheduler(&self, context: &SecurityTaskGetSchedulerContext<'_>) -> AxResult<()> {
        SecurityModule::task_getscheduler(self, context)
    }

    fn task_getscheduler_with_credential_state(
        &self,
        context: &SecurityTaskGetSchedulerContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::task_getscheduler_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
            owned_credential_state(self, target_state)?,
        )
    }

    fn task_getpgid(&self, context: &SecurityTaskGetpgidContext<'_>) -> AxResult<()> {
        SecurityModule::task_getpgid(self, context)
    }

    fn task_getpgid_with_credential_state(
        &self,
        context: &SecurityTaskGetpgidContext<'_>,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::task_getpgid_with_credential_state(
            self,
            context,
            owned_credential_state(self, target_state)?,
        )
    }

    fn signal(&self, context: &SecuritySignalContext<'_>) -> AxResult<()> {
        SecurityModule::signal(self, context)
    }

    fn signal_with_credential_state(
        &self,
        context: &SecuritySignalContext<'_>,
        actor_state: &dyn ErasedOwnedCredentialState,
        target_state: &dyn ErasedOwnedCredentialState,
    ) -> AxResult<()> {
        SecurityModule::signal_with_credential_state(
            self,
            context,
            owned_credential_state(self, actor_state)?,
            owned_credential_state(self, target_state)?,
        )
    }
}

#[cfg(test)]
pub(crate) fn assert_post_commit_callback_locks_released() {
    assert!(!crate::task::creds::credential_writer_lock_held());
    assert!(!crate::task::creds::credential_publication_lock_held());
    assert!(!crate::task::process::process_security_lock_held());
    assert!(!crate::task::process::process_image_lock_held());
    assert!(!crate::task::process::group_leader_lock_held());
    assert!(!crate::task::process::ptrace_action_lock_held());
    assert!(!crate::task::ops::task_alias_lock_held());
}
