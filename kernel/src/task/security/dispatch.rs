//! The crate-facing dispatch entry points.
//!
//! Every hook the rest of the kernel calls enters here. These functions own
//! one requirement — that a registry has actually been published — and
//! nothing else: policy lives in the modules, shape lives in the contexts.

use super::*;

/// Runs typed inode-permission hooks after the caller has completed DAC
/// admission over the exact frozen object. The first denial is returned
/// immediately.
///
/// The current call-site contract is the open/pathwalk vertical slice, not a
/// claim that every VFS permission path has already migrated. Dispatch may be
/// inside filesystem-context/pathwalk lock domains, so hooks are
/// allocation-free, nonblocking, and forbidden from VFS/current/credential
/// reentry.
pub(crate) fn dispatch_inode_permission(
    context: &InodePermissionSecurityContext<'_, '_>,
) -> AxResult<()> {
    #[cfg(feature = "bpf")]
    crate::bpf::run_lsm_hook(1)?;
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_permission_with_credential_state(context)
}

/// Linear proof that every inode-xattr pre-hook admitted one exact frozen
/// operation and that the complete actor module-state vector was preflighted.
///
/// Dropping this token is the provider-failure or caller-abort path and emits
/// no post notification. Only [`Self::committed`] emits the success-only post
/// pass, using the exact context admitted before provider dispatch.
#[must_use = "an admitted inode xattr operation must either abort or publish its post notification"]
pub(crate) struct InodeXattrSecurityAdmission<'context, 'location> {
    pub(super) registry: FrozenSecurityRegistry,
    pub(super) context: InodeXattrSecurityContext<'context, 'location>,
}

impl<'context, 'location> InodeXattrSecurityAdmission<'context, 'location> {
    /// Emits the ordered, infallible post-xattr pass after provider success.
    /// No actor, credential-state, DAC, namespace, inode, or operation input is
    /// resampled between the pre and post callbacks.
    pub(crate) fn committed(self) {
        self.registry
            .registry()
            .notify_inode_post_xattr_with_credential_state(&self.context);
    }
}

/// Runs the ordered, deny-first inode-xattr hook stack and returns a linear
/// admission which the caller may commit only after the provider succeeds.
pub(crate) fn dispatch_inode_xattr<'context, 'location>(
    context: InodeXattrSecurityContext<'context, 'location>,
) -> AxResult<InodeXattrSecurityAdmission<'context, 'location>> {
    let registry = context.actor().security().registry();
    registry
        .registry()
        .dispatch_inode_xattr_with_credential_state(&context)?;
    Ok(InodeXattrSecurityAdmission { registry, context })
}

/// Linear proof that every inode-setattr pre-hook admitted one exact frozen
/// proposal and that the complete actor module-state vector was preflighted.
///
/// Dropping this token is the ordinary backend-failure path and emits no post
/// notification. Only [`Self::committed`] can construct and publish the
/// infallible post context, so a caller cannot accidentally notify after a
/// denied or failed mutation.
#[must_use = "an admitted inode setattr must either abort or publish its post notification"]
pub(crate) struct InodeSetattrSecurityAdmission<'context, 'location> {
    pub(super) registry: FrozenSecurityRegistry,
    pub(super) context: InodeSetattrSecurityContext<'context, 'location>,
}

impl<'context, 'location> InodeSetattrSecurityAdmission<'context, 'location> {
    /// Emits the ordered, infallible post-setattr pass for one exact committed
    /// inode snapshot. No registry, credential-state, DAC, or VFS revalidation
    /// occurs after the backend has reported success.
    pub(crate) fn committed(self, committed_object: InodeSetattrCommittedSecurityRef<'location>) {
        let context = InodePostSetattrSecurityContext::new(
            self.context.actor(),
            self.context.dac_credential(),
            self.context.target_owner_user_ns(),
            committed_object,
            self.context.proposal(),
        );
        self.registry
            .registry()
            .notify_inode_post_setattr_with_credential_state(&context);
    }
}

/// Runs the ordered, deny-first inode-setattr hook stack and returns a linear
/// admission retaining every frozen input required by the success-only post
/// phase. The context is consumed so the token owns the old inode snapshot
/// rather than borrowing a construction-frame local.
pub(crate) fn dispatch_inode_setattr<'context, 'location>(
    context: InodeSetattrSecurityContext<'context, 'location>,
) -> AxResult<InodeSetattrSecurityAdmission<'context, 'location>> {
    let registry = context.actor().security().registry();
    registry
        .registry()
        .dispatch_inode_setattr_with_credential_state(&context)?;
    Ok(InodeSetattrSecurityAdmission { registry, context })
}

/// Runs typed regular-file creation hooks for one frozen planned entry. The
/// first denial is returned before VFS creates the object.
pub(crate) fn dispatch_inode_create(
    context: &InodeCreateSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_create_with_credential_state(context)
}

/// Runs typed directory creation hooks for one frozen planned entry. The first
/// denial is returned before VFS creates the object.
pub(crate) fn dispatch_inode_mkdir(
    context: &InodeMkdirSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_mkdir_with_credential_state(context)
}

/// Runs typed special-node creation hooks for one frozen planned entry and
/// already-validated kind/mode/rdev operation.
pub(crate) fn dispatch_inode_mknod(
    context: &InodeMknodSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_mknod_with_credential_state(context)
}

/// Runs typed symbolic-link creation hooks for one frozen planned entry and
/// exact target. The first denial is returned before filesystem publication.
pub(crate) fn dispatch_inode_symlink(
    context: &InodeSymlinkSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_symlink_with_credential_state(context)
}

/// Runs typed hard-link hooks for one frozen source and prospective
/// destination. The first denial is returned before filesystem publication.
pub(crate) fn dispatch_inode_link(context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_link_with_credential_state(context)
}

/// Runs typed non-directory removal hooks for one frozen parent, victim, and
/// final name. The first denial is returned before filesystem publication.
pub(crate) fn dispatch_inode_unlink(
    context: &InodeUnlinkSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    #[cfg(feature = "bpf")]
    crate::bpf::run_lsm_hook(2)?;
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_unlink_with_credential_state(context)
}

/// Runs typed directory-removal hooks for one frozen parent, directory victim,
/// and final name. This entry point cannot dispatch the unlink hook family.
pub(crate) fn dispatch_inode_rmdir(
    context: &InodeRmdirSecurityContext<'_, '_, '_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_rmdir_with_credential_state(context)
}

/// Runs one typed rename leaf over the exact frozen old and new object roles.
/// The first denial stops dispatch before the caller publishes a namespace
/// mutation. Exchange's reverse-then-forward sequencing remains explicit at
/// the transaction layer rather than being inferred here.
pub(crate) fn dispatch_inode_rename(
    context: &InodeRenameSecurityContext<'_, '_, '_, '_>,
) -> AxResult<()> {
    #[cfg(feature = "bpf")]
    crate::bpf::run_lsm_hook(3)?;
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_inode_rename_with_credential_state(context)
}

/// Runs typed file-open hooks for one already-resolved, still-unpublished open
/// transaction. The first denial is returned immediately.
///
/// This entry point serves the current open vertical slice rather than every
/// possible kernel-internal file construction. Callers invoke it before fd,
/// persistent executable-write reservation, fanotify open permission, POSIX
/// lease conflict handling, filesystem-open, or truncate side effects become
/// visible. Hooks are allocation-free, nonblocking, and forbidden from
/// VFS/current/credential or nested open reentry.
pub(crate) fn dispatch_file_open(context: &FileOpenSecurityContext<'_, '_>) -> AxResult<()> {
    #[cfg(feature = "bpf")]
    crate::bpf::run_lsm_hook(6)?;
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_file_open_with_credential_state(context)
}

/// Runs one typed socket hook stack in declaration order after the caller has
/// completed required usercopy and before backend mutation or publication.
pub(crate) fn dispatch_socket(context: &SocketSecurityContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_socket_with_credential_state(context)
}

/// Per-registry observer used by socketpair adapter tests.
///
/// The probe is owned by one test credential, so parallel tests cannot replace
/// a global callback or consume another test's lifecycle events.
#[cfg(test)]
pub(crate) struct SocketPairSecurityTestProbe {
    pub(super) net_namespace: Arc<crate::task::NetworkNamespace>,
    pub(super) deny_pair: bool,
    pub(super) step: core::sync::atomic::AtomicUsize,
    pub(super) create_calls: core::sync::atomic::AtomicUsize,
    pub(super) post_create_calls: core::sync::atomic::AtomicUsize,
    pub(super) pair_calls: core::sync::atomic::AtomicUsize,
    pub(super) first_ofd: core::sync::atomic::AtomicU64,
    pub(super) second_ofd: core::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl SocketPairSecurityTestProbe {
    pub(crate) fn new(
        net_namespace: Arc<crate::task::NetworkNamespace>,
        deny_pair: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            net_namespace,
            deny_pair,
            step: core::sync::atomic::AtomicUsize::new(0),
            create_calls: core::sync::atomic::AtomicUsize::new(0),
            post_create_calls: core::sync::atomic::AtomicUsize::new(0),
            pair_calls: core::sync::atomic::AtomicUsize::new(0),
            first_ofd: core::sync::atomic::AtomicU64::new(0),
            second_ofd: core::sync::atomic::AtomicU64::new(0),
        })
    }

    pub(super) fn assert_spec(spec: SocketCreateSpec) {
        assert_eq!(spec.family(), linux_raw_sys::net::AF_PACKET as i32);
        assert!(matches!(
            spec.socket_type(),
            value if value == linux_raw_sys::net::SOCK_RAW as i32
                || value == linux_raw_sys::net::SOCK_DGRAM as i32
        ));
        assert!(!spec.kernel_origin());
    }

    pub(super) fn advance(&self, expected: usize, next: usize) {
        assert_eq!(
            self.step.compare_exchange(
                expected,
                next,
                core::sync::atomic::Ordering::SeqCst,
                core::sync::atomic::Ordering::SeqCst,
            ),
            Ok(expected),
            "socketpair security hook order changed"
        );
    }

    pub(super) fn observe_create(&self, spec: SocketCreateSpec) {
        Self::assert_spec(spec);
        match self.step.load(core::sync::atomic::Ordering::SeqCst) {
            0 => self.advance(0, 1),
            2 => self.advance(2, 3),
            step => panic!("socketpair create hook reached at step {step}"),
        }
        self.create_calls
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn assert_packet_socket(&self, socket: &SocketSecurityRef<'_>) -> u64 {
        assert_eq!(socket.backend(), crate::file::SocketBackendKind::Packet);
        let socket_namespace = socket
            .net_namespace()
            .expect("packet socket security ref carries a network namespace");
        assert!(Arc::ptr_eq(socket_namespace, &self.net_namespace));
        socket.ofd_identity()
    }

    pub(super) fn observe_post_create(
        &self,
        socket: &SocketSecurityRef<'_>,
        spec: SocketCreateSpec,
    ) {
        Self::assert_spec(spec);
        let ofd = self.assert_packet_socket(socket);
        match self.step.load(core::sync::atomic::Ordering::SeqCst) {
            1 => {
                self.first_ofd
                    .store(ofd, core::sync::atomic::Ordering::SeqCst);
                self.advance(1, 2);
            }
            3 => {
                self.second_ofd
                    .store(ofd, core::sync::atomic::Ordering::SeqCst);
                self.advance(3, 4);
            }
            step => panic!("socketpair post-create hook reached at step {step}"),
        }
        self.post_create_calls
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn observe_pair(
        &self,
        first: &SocketSecurityRef<'_>,
        second: &SocketSecurityRef<'_>,
    ) -> AxResult<()> {
        let first_ofd = self.assert_packet_socket(first);
        let second_ofd = self.assert_packet_socket(second);
        assert_ne!(first_ofd, second_ofd);
        assert_eq!(
            first_ofd,
            self.first_ofd.load(core::sync::atomic::Ordering::SeqCst)
        );
        assert_eq!(
            second_ofd,
            self.second_ofd.load(core::sync::atomic::Ordering::SeqCst)
        );
        self.advance(4, 0);
        self.pair_calls
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        if self.deny_pair {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }

    pub(crate) fn assert_complete_cycles(&self, cycles: usize) {
        assert_eq!(self.step.load(core::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            self.create_calls.load(core::sync::atomic::Ordering::SeqCst),
            cycles * 2
        );
        assert_eq!(
            self.post_create_calls
                .load(core::sync::atomic::Ordering::SeqCst),
            cycles * 2
        );
        assert_eq!(
            self.pair_calls.load(core::sync::atomic::Ordering::SeqCst),
            cycles
        );
    }
}

#[cfg(test)]
pub(crate) struct SocketPairSecurityTestModule {
    pub(super) probe: Arc<SocketPairSecurityTestProbe>,
}

#[cfg(test)]
impl SecurityModule for SocketPairSecurityTestModule {
    const KEY: ModuleKey = ModuleKey(0x736f_636b_7061_6972);
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        unreachable!("socketpair test module is registered as an initialized instance")
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

    fn socket(&self, context: &SocketSecurityContext<'_>) -> AxResult<()> {
        match context.operation() {
            SocketSecurityOperation::Create(operation) => {
                self.probe.observe_create(operation.spec());
                Ok(())
            }
            SocketSecurityOperation::PostCreate(operation) => {
                self.probe
                    .observe_post_create(operation.created_socket(), operation.spec());
                Ok(())
            }
            SocketSecurityOperation::Pair(operation) => self
                .probe
                .observe_pair(operation.first_socket(), operation.second_socket()),
            _ => panic!("socketpair test credential received an unrelated socket hook"),
        }
    }
}

#[cfg(test)]
pub(crate) fn socket_pair_security_test_credential(
    user_namespace: Arc<UserNamespace>,
    probe: Arc<SocketPairSecurityTestProbe>,
) -> Arc<Cred> {
    let mut builder = SecurityRegistryBuilder::try_new()
        .expect("socketpair test registry allocation failed")
        .try_register_commoncap()
        .expect("socketpair test commoncap registration failed");
    builder
        .try_register_initialized(SocketPairSecurityTestModule { probe })
        .expect("socketpair test module registration failed");
    let registry = Box::new(builder.freeze());
    Cred::try_root_with_registry(FrozenSecurityRegistry(Box::leak(registry)), user_namespace)
        .expect("socketpair test credential construction failed")
}

pub(crate) fn mmap_memory_protection(flags: MappingFlags) -> MemoryProtection {
    let mut bits = 0;
    if flags.contains(MappingFlags::READ) {
        bits |= MemoryProtection::READ.bits();
    }
    if flags.contains(MappingFlags::WRITE) {
        bits |= MemoryProtection::WRITE.bits();
    }
    if flags.contains(MappingFlags::EXECUTE) {
        bits |= MemoryProtection::EXECUTE.bits();
    }
    MemoryProtection::try_from_bits(bits).expect("adapter emits only PROT_READ/WRITE/EXEC bits")
}

/// Runs the typed `mmap_file` stack over either an anonymous target or the
/// exact retained file/OFD selected by the syscall adapter.
pub(crate) fn mmap_file(
    actor: &Cred,
    target: Option<(&Arc<UserNamespace>, &FileHandle<File>)>,
    requested: MappingFlags,
    effective: MappingFlags,
    raw_flags: usize,
) -> AxResult<()> {
    let target = match target {
        Some((filesystem_owner_user_ns, file)) => {
            MmapFileTarget::File(MmapFileSecurityRef::new(filesystem_owner_user_ns, file))
        }
        None => MmapFileTarget::Anonymous,
    };
    let operation = MmapFileOperation::new(
        mmap_memory_protection(requested),
        mmap_memory_protection(effective),
        MmapFileFlags::from_raw(raw_flags),
    );
    let context = MmapFileContext::new(actor.core(), target, operation);
    actor
        .security()
        .registry()
        .registry()
        .dispatch_mmap_file_with_credential_state(actor, &context)
}

/// Runs the typed `mmap_addr` stack over the final candidate in one exact
/// retained address-space image.
pub(crate) fn mmap_addr(
    actor: &Cred,
    image_owner_user_ns: &Arc<UserNamespace>,
    image: &Arc<Mutex<AddrSpace>>,
    final_address: VirtAddr,
) -> AxResult<()> {
    let image = MmapImageSecurityRef::from_arc(image);
    dispatch_mmap_addr(actor, image_owner_user_ns, &image, final_address)
}

pub(crate) fn dispatch_mmap_addr(
    actor: &Cred,
    image_owner_user_ns: &Arc<UserNamespace>,
    image: &MmapImageSecurityRef,
    final_address: VirtAddr,
) -> AxResult<()> {
    let context = MmapAddressContext::new(
        actor.core(),
        image_owner_user_ns,
        image,
        final_address.as_usize(),
    );
    actor
        .security()
        .registry()
        .registry()
        .dispatch_mmap_addr_with_credential_state(actor, &context)
}

/// Runs the typed `file_mprotect` stack for one exact pre-change VMA segment.
/// The caller owns the prepared transaction and commits only after every
/// segment has passed this dispatch.
pub(crate) fn file_mprotect<'segment>(
    actor: &Cred,
    image_owner_user_ns: &Arc<UserNamespace>,
    segment: PreparedProtectSegment<'segment>,
    requested: MappingFlags,
    effective: MappingFlags,
) -> AxResult<()> {
    let context = FileMprotectContext::new(
        actor.core(),
        image_owner_user_ns,
        &segment,
        mmap_memory_protection(requested),
        mmap_memory_protection(effective),
    );
    actor
        .security()
        .registry()
        .registry()
        .dispatch_file_mprotect_with_credential_state(actor, &context)
}

/// Runs Linux commoncap first, then lets the exact actor's frozen module stack
/// narrow the successful decision in declaration order.
///
/// Invalid raw numbers and commoncap denials return before registry lookup or
/// any module callback. Complete composite-state validation likewise finishes
/// before the mandatory commoncap module and later policy modules run.
pub(crate) fn authorize_capability_with_operation(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
    operation: CapabilitySecurityOperation,
) -> AxResult<()> {
    let capability = CapabilityNumber::try_new(raw_capability).ok_or(AxError::InvalidInput)?;
    let context =
        external_authorize_capability_core(actor.core(), target_user_ns, capability, operation)
            .map_err(authorization_error)?;
    actor
        .security()
        .registry()
        .registry()
        .dispatch_capable_with_credential_state(actor, &context)
}

/// Ordinary audited capability check used by every general kernel capability
/// entry point, including `task::access::ns_capable`.
pub(in crate::task) fn capable(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
) -> bool {
    authorize_capability_with_operation(
        actor,
        target_user_ns,
        raw_capability,
        CapabilitySecurityOperation::Use,
    )
    .is_ok()
}

/// Set-ID-family capability check. Keeping this helper specific prevents an
/// arbitrary caller from relabeling an ordinary check as `CAP_OPT_INSETID`.
pub(in crate::task) fn capable_for_setid(
    actor: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
) -> bool {
    authorize_capability_with_operation(
        actor,
        target_user_ns,
        raw_capability,
        CapabilitySecurityOperation::SetId,
    )
    .is_ok()
}

/// Returns whether an LSM lockdown policy denies raw x86 I/O-port access.
///
/// This is deliberately called after the caller's CAP_SYS_RAWIO check, just
/// like Linux's `security_locked_down(LOCKDOWN_IOPORT)` path.
pub(in crate::task) fn locked_down_ioport(actor: &Cred) -> bool {
    actor
        .security()
        .registry()
        .registry()
        .dispatch_locked_down_ioport(actor)
        .is_err()
}

/// Linux `security_kernel_load_data` plus lockdown admission for kexec.
pub(crate) fn authorize_kernel_load_data(
    actor: &Cred,
    kind: KernelLoadKind,
    from_file: bool,
) -> AxResult<()> {
    actor
        .security()
        .registry()
        .registry()
        .dispatch_kernel_load_data(actor, kind, from_file)
}

/// Checks namespace-creation authority carried by one exact prepared child.
/// Commoncap evaluates the proposed credential, while stacked modules receive
/// both the live source state and the still-private proposed state.
pub(crate) fn prepared_credential_namespace_capable(
    source: &Cred,
    proposed: &Cred,
    target_user_ns: &Arc<UserNamespace>,
    raw_capability: u32,
) -> bool {
    let authorize = || -> AxResult<()> {
        let registry = source.security().registry();
        registry
            .registry()
            .validate_credential_pair(source, proposed)?;
        if !matches!(
            proposed
                .security()
                .prepared_transition_from(source.security())?,
            CredentialStateTransition::Fork | CredentialStateTransition::UserNamespace
        ) {
            return Err(AxError::BadState);
        }
        let capability = CapabilityNumber::try_new(raw_capability).ok_or(AxError::InvalidInput)?;
        let context = external_authorize_prepared_credential_capability_core(
            source.core(),
            proposed.core(),
            target_user_ns,
            capability,
            PreparedCredentialCapabilityOperation::NamespaceCreate,
        )
        .map_err(authorization_error)?;
        registry
            .registry()
            .dispatch_prepared_credential_capable(source, proposed, &context)
    };
    authorize().is_ok()
}

/// Runs the frozen ptrace access hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_access(context: &PtraceAccessContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_ptrace_access_with_credential_state(context)
}

/// Runs the frozen traceme hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_ptrace_traceme(context: &PtraceTracemeContext<'_>) -> AxResult<()> {
    context
        .parent_actor()
        .security()
        .registry()
        .registry()
        .dispatch_ptrace_traceme_with_credential_state(context)
}

/// Runs the frozen exec-credential hooks in declaration order.
/// The first denial aborts the still-unpublished prepared credential.
pub(crate) fn dispatch_exec_credential(
    context: &ExecCredentialSecurityContext<'_>,
) -> AxResult<()> {
    #[cfg(feature = "bpf")]
    crate::bpf::run_lsm_hook(4)?;
    context
        .old()
        .security()
        .registry()
        .registry()
        .dispatch_exec_credential_with_credential_state(context)
}

/// Runs typed executable-component hooks for the already-resolved object in
/// declaration order. Denial happens before the loader consumes that
/// component and drops every transient executable lease on unwind.
pub(crate) fn dispatch_exec_executable(
    context: &ExecExecutableSecurityContext<'_>,
) -> AxResult<()> {
    #[cfg(feature = "bpf")]
    crate::bpf::run_lsm_hook(5)?;
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_exec_executable_with_credential_state(context)
}

/// Runs the frozen scheduler hooks in declaration order.
/// The first denial is returned immediately.
pub(crate) fn dispatch_scheduler(context: &SecuritySchedulerContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_scheduler_with_credential_state(context)
}

/// Runs Linux `security_task_getsid` hooks for one already-resolved target.
pub(crate) fn dispatch_task_getsid(context: &SecurityTaskGetsidContext<'_>) -> AxResult<()> {
    context
        .target()
        .security()
        .registry()
        .registry()
        .dispatch_task_getsid_with_credential_state(context)
}

/// Runs Linux `security_task_getscheduler` hooks for one already-resolved
/// actor/target credential snapshot.
pub(crate) fn dispatch_task_getscheduler(
    context: &SecurityTaskGetSchedulerContext<'_>,
) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_task_getscheduler_with_credential_state(context)
}

/// Runs Linux `security_task_getpgid` hooks for one already-resolved target.
pub(crate) fn dispatch_task_getpgid(context: &SecurityTaskGetpgidContext<'_>) -> AxResult<()> {
    context
        .target()
        .security()
        .registry()
        .registry()
        .dispatch_task_getpgid_with_credential_state(context)
}

/// Runs the frozen signal policy hooks after Linux core signal permission has
/// admitted the exact actor/target pair. The first denial is returned without
/// invoking later modules.
pub(crate) fn dispatch_signal(context: &SecuritySignalContext<'_>) -> AxResult<()> {
    context
        .actor()
        .security()
        .registry()
        .registry()
        .dispatch_signal_with_credential_state(context)
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RenameSecurityTestObjectFacts {
    pub(super) identity: InodeIdentity,
    pub(super) mode: u16,
    pub(super) node_kind: NodeType,
    pub(super) uid: u32,
    pub(super) gid: u32,
    pub(super) size: u64,
}

#[cfg(test)]
impl RenameSecurityTestObjectFacts {
    pub(crate) fn from_location(location: &Location) -> AxResult<Self> {
        let metadata = location.metadata()?;
        let object = InodeSecurityRef::new(location, &metadata);
        Ok(Self::from_object(&object))
    }

    pub(super) fn from_object(object: &InodeSecurityRef<'_>) -> Self {
        Self {
            identity: object.identity(),
            mode: object.mode(),
            node_kind: object.node_kind(),
            uid: object.uid(),
            gid: object.gid(),
            size: object.size(),
        }
    }

    pub(super) fn assert_matches(&self, object: &InodeSecurityRef<'_>) {
        assert_eq!(object.identity(), self.identity);
        assert_eq!(object.mode(), self.mode);
        assert_eq!(object.node_kind(), self.node_kind);
        assert_eq!(object.uid(), self.uid);
        assert_eq!(object.gid(), self.gid);
        assert_eq!(object.size(), self.size);
    }
}

/// Immutable test-only expectation consumed by a per-test rename probe.
///
/// Each namespace test owns its probe through an `Arc`; there is no shared
/// global callback or mutable expected-value slot, so parallel tests cannot
/// observe or reset one another's state.
#[cfg(test)]
pub(crate) struct RenameSecurityTestExpectation {
    pub(super) old_parent: RenameSecurityTestObjectFacts,
    pub(super) source: RenameSecurityTestObjectFacts,
    pub(super) old_name: alloc::string::String,
    pub(super) new_parent: RenameSecurityTestObjectFacts,
    pub(super) replaced: Option<RenameSecurityTestObjectFacts>,
    pub(super) new_name: alloc::string::String,
}

#[cfg(test)]
impl RenameSecurityTestExpectation {
    pub(crate) fn new(
        old_parent: &Location,
        source: &Location,
        old_name: &str,
        new_parent: &Location,
        replaced: Option<&Location>,
        new_name: &str,
    ) -> AxResult<Self> {
        Ok(Self {
            old_parent: RenameSecurityTestObjectFacts::from_location(old_parent)?,
            source: RenameSecurityTestObjectFacts::from_location(source)?,
            old_name: alloc::string::String::from(old_name),
            new_parent: RenameSecurityTestObjectFacts::from_location(new_parent)?,
            replaced: replaced
                .map(RenameSecurityTestObjectFacts::from_location)
                .transpose()?,
            new_name: alloc::string::String::from(new_name),
        })
    }

    pub(super) fn assert_matches(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) {
        self.old_parent.assert_matches(context.old_parent_object());
        self.source
            .assert_matches(context.old_entry_object().target_object());
        assert_eq!(
            context.old_entry_object().name().as_bytes(),
            self.old_name.as_bytes()
        );
        assert!(core::ptr::eq(
            context.old_parent_object(),
            context.old_entry_object().parent_object()
        ));

        self.new_parent.assert_matches(context.new_parent_object());
        assert_eq!(
            context.new_entry_object().name().as_bytes(),
            self.new_name.as_bytes()
        );
        assert!(core::ptr::eq(
            context.new_parent_object(),
            context.new_entry_object().parent_object()
        ));
        match (&self.replaced, context.new_entry_object().target_object()) {
            (Some(expected), Some(actual)) => expected.assert_matches(actual),
            (None, None) => {}
            _ => panic!("rename destination presence did not match the frozen expectation"),
        }
    }
}

#[cfg(test)]
pub(crate) struct RenameSecurityTestProbe {
    pub(super) expectation: RenameSecurityTestExpectation,
    pub(super) deny: bool,
    pub(super) calls: core::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl RenameSecurityTestProbe {
    pub(crate) fn new(expectation: RenameSecurityTestExpectation, deny: bool) -> Arc<Self> {
        Arc::new(Self {
            expectation,
            deny,
            calls: core::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(core::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
pub(crate) struct RenameSecurityTestModule<const KEY: u64> {
    pub(super) probe: Arc<RenameSecurityTestProbe>,
}

#[cfg(test)]
impl<const KEY: u64> SecurityModule for RenameSecurityTestModule<KEY> {
    const KEY: ModuleKey = ModuleKey(KEY);
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        unreachable!("rename test modules are registered as initialized instances")
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

    fn inode_rename(&self, context: &InodeRenameSecurityContext<'_, '_, '_, '_>) -> AxResult<()> {
        self.probe.expectation.assert_matches(context);
        self.probe
            .calls
            .fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        if self.probe.deny {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) fn rename_security_test_registry(
    first: Arc<RenameSecurityTestProbe>,
    second: Option<Arc<RenameSecurityTestProbe>>,
) -> FrozenSecurityRegistry {
    const FIRST_KEY: u64 = 0x7265_6e61_6d65_0100;
    const SECOND_KEY: u64 = 0x7265_6e61_6d65_0200;

    let mut builder = SecurityRegistryBuilder::try_new()
        .expect("rename test registry allocation failed")
        .try_register_commoncap()
        .expect("rename test commoncap registration failed");
    builder
        .try_register_initialized(RenameSecurityTestModule::<FIRST_KEY> { probe: first })
        .expect("rename test first module registration failed");
    if let Some(second) = second {
        builder
            .try_register_initialized(RenameSecurityTestModule::<SECOND_KEY> { probe: second })
            .expect("rename test second module registration failed");
    }
    let registry = Box::new(builder.freeze());
    FrozenSecurityRegistry(Box::leak(registry))
}

#[cfg(test)]
pub(crate) fn rename_security_test_credential(
    namespace: Arc<UserNamespace>,
    first: Arc<RenameSecurityTestProbe>,
    second: Option<Arc<RenameSecurityTestProbe>>,
) -> Arc<Cred> {
    Cred::try_root_with_registry(rename_security_test_registry(first, second), namespace)
        .expect("rename test credential construction failed")
}

#[cfg(test)]
pub(crate) fn rename_security_test_unprivileged_credential(
    namespace: Arc<UserNamespace>,
    first: Arc<RenameSecurityTestProbe>,
    second: Option<Arc<RenameSecurityTestProbe>>,
    uid: u32,
    gid: u32,
) -> Arc<Cred> {
    let root = rename_security_test_credential(namespace, first, second);
    let slot = crate::task::creds::CredentialSlot::new(root);
    let mut update = slot.prepare();
    let uid = crate::task::Kuid::from_raw(uid).expect("rename test uid must be representable");
    let gid = crate::task::Kgid::from_raw(gid).expect("rename test gid must be representable");
    update.builder.ids = crate::task::Credentials {
        ruid: uid,
        euid: uid,
        suid: uid,
        fsuid: uid,
        rgid: gid,
        egid: gid,
        sgid: gid,
        fsgid: gid,
    };
    let caps = update.builder.caps;
    update.builder.caps = crate::task::creds::capability_state_for_test(
        [0; crate::task::creds::CAPABILITY_WORDS],
        [0; crate::task::creds::CAPABILITY_WORDS],
        [0; crate::task::creds::CAPABILITY_WORDS],
        caps.bounding(),
        [0; crate::task::creds::CAPABILITY_WORDS],
        caps.securebits(),
    );
    update
        .finish()
        .expect("rename test unprivileged credential preparation failed")
        .commit()
}

#[cfg(test)]
pub(crate) fn malformed_rename_security_test_credential(
    namespace: Arc<UserNamespace>,
    probe: Arc<RenameSecurityTestProbe>,
) -> Arc<Cred> {
    const WRONG_KEY: u64 = 0x7265_6e61_6d65_0300;

    let registry = rename_security_test_registry(probe.clone(), None);
    let actor = Cred::try_root_with_registry(registry, namespace)
        .expect("rename malformed test actor construction failed");
    let mut malformed = registry
        .registry()
        .try_init_credential_state(registry, actor.core())
        .expect("rename malformed test state construction failed");
    malformed.slots[1].erased = try_own_credential_state(
        Arc::new(RenameSecurityTestModule::<WRONG_KEY> { probe }),
        (),
    )
    .expect("rename malformed test state ownership failed");
    Cred::try_from_prepared_parts(actor.core_arc().clone(), malformed)
        .expect("rename malformed test credential construction failed")
}

/// Typed leaf expected from one named-create transaction test.
///
/// This deliberately mirrors the public hook family instead of collapsing all
/// creates into one untyped callback.  A transaction test can therefore prove
/// both that the final hook is reached exactly once and that it is reached
/// through the Linux hook selected for the frozen inode kind.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NamedCreateSecurityTestLeaf {
    RegularFile {
        mode: u16,
    },
    Directory {
        mode: u16,
    },
    Mknod {
        kind: InodeMknodKind,
        mode: u16,
        rdev: Option<u64>,
    },
    Symlink {
        target: alloc::string::String,
    },
}

/// Immutable per-test expectation for one planned named entry.
#[cfg(test)]
pub(crate) struct NamedCreateSecurityTestExpectation {
    pub(super) parent: RenameSecurityTestObjectFacts,
    pub(super) name: alloc::string::String,
    pub(super) leaf: NamedCreateSecurityTestLeaf,
}

#[cfg(test)]
impl NamedCreateSecurityTestExpectation {
    pub(crate) fn new(
        parent: &Location,
        name: &str,
        leaf: NamedCreateSecurityTestLeaf,
    ) -> AxResult<Self> {
        Ok(Self {
            parent: RenameSecurityTestObjectFacts::from_location(parent)?,
            name: alloc::string::String::from(name),
            leaf,
        })
    }

    pub(super) fn assert_parent(&self, object: &InodeSecurityRef<'_>) {
        self.parent.assert_matches(object);
    }

    pub(super) fn assert_planned(&self, object: &PlannedInodeSecurityRef<'_, '_>) {
        self.assert_parent(object.parent_object());
        assert_eq!(object.name().as_bytes(), self.name.as_bytes());
    }
}

/// Per-test named-create security probe.
///
/// All observations are owned by the credential registry used by that one
/// test.  There is no mutable global callback or reset protocol, so separate
/// namespace tests remain safe under the Rust test runner's parallel schedule.
#[cfg(test)]
pub(crate) struct NamedCreateSecurityTestProbe {
    pub(super) expectation: NamedCreateSecurityTestExpectation,
    pub(super) deny_permission: bool,
    pub(super) deny_leaf: bool,
    pub(super) permission_calls: core::sync::atomic::AtomicUsize,
    pub(super) leaf_calls: core::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl NamedCreateSecurityTestProbe {
    pub(crate) fn new(
        expectation: NamedCreateSecurityTestExpectation,
        deny_permission: bool,
        deny_leaf: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            expectation,
            deny_permission,
            deny_leaf,
            permission_calls: core::sync::atomic::AtomicUsize::new(0),
            leaf_calls: core::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub(crate) fn permission_calls(&self) -> usize {
        self.permission_calls
            .load(core::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn leaf_calls(&self) -> usize {
        self.leaf_calls.load(core::sync::atomic::Ordering::SeqCst)
    }

    pub(super) fn observe_permission(
        &self,
        context: &InodePermissionSecurityContext<'_, '_>,
    ) -> AxResult<()> {
        self.expectation.assert_parent(context.target_object());
        assert_eq!(
            context.access(),
            InodePermissionAccess::WRITE | InodePermissionAccess::EXECUTE
        );
        assert_eq!(
            self.permission_calls
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst),
            0,
            "named-create parent permission hook ran more than once"
        );
        if self.deny_permission {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }

    pub(super) fn begin_leaf(&self, planned: &PlannedInodeSecurityRef<'_, '_>) {
        self.expectation.assert_planned(planned);
        assert_eq!(
            self.permission_calls(),
            1,
            "named-create leaf ran before its one parent permission hook"
        );
        assert_eq!(
            self.leaf_calls
                .fetch_add(1, core::sync::atomic::Ordering::SeqCst),
            0,
            "named-create typed leaf ran more than once"
        );
    }

    pub(super) fn finish_leaf(&self) -> AxResult<()> {
        if self.deny_leaf {
            Err(AxError::PermissionDenied)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) struct NamedCreateSecurityTestModule<const KEY: u64> {
    pub(super) probe: Arc<NamedCreateSecurityTestProbe>,
}

#[cfg(test)]
impl<const KEY: u64> SecurityModule for NamedCreateSecurityTestModule<KEY> {
    const KEY: ModuleKey = ModuleKey(KEY);
    type CredentialState = ();

    fn try_boot_init() -> Result<Self, RegistryBuildError> {
        unreachable!("named-create test modules are registered as initialized instances")
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

    fn inode_permission(&self, context: &InodePermissionSecurityContext<'_, '_>) -> AxResult<()> {
        self.probe.observe_permission(context)
    }

    fn inode_create(&self, context: &InodeCreateSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::RegularFile { mode } = &self.probe.expectation.leaf else {
            panic!("regular-file create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.mode().bits(), *mode);
        self.probe.finish_leaf()
    }

    fn inode_mkdir(&self, context: &InodeMkdirSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::Directory { mode } = &self.probe.expectation.leaf else {
            panic!("directory create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.mode().bits(), *mode);
        self.probe.finish_leaf()
    }

    fn inode_mknod(&self, context: &InodeMknodSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::Mknod { kind, mode, rdev } = &self.probe.expectation.leaf
        else {
            panic!("special-node create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.operation().kind(), *kind);
        assert_eq!(context.operation().mode().bits(), *mode);
        assert_eq!(context.operation().rdev(), *rdev);
        self.probe.finish_leaf()
    }

    fn inode_symlink(&self, context: &InodeSymlinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        self.probe.begin_leaf(context.new_entry_object());
        let NamedCreateSecurityTestLeaf::Symlink { target } = &self.probe.expectation.leaf else {
            panic!("symlink create dispatched the wrong named-create test leaf")
        };
        assert_eq!(context.symlink_target().as_bytes(), target.as_bytes());
        self.probe.finish_leaf()
    }

    fn inode_link(&self, context: &InodeLinkSecurityContext<'_, '_, '_>) -> AxResult<()> {
        // Destination-order tests also use this probe as a canary for an
        // unexpectedly reached hard-link leaf.  The planned destination is
        // still checked against the exact per-test expectation.
        self.probe.begin_leaf(context.new_entry_object());
        self.probe.finish_leaf()
    }
}

#[cfg(test)]
pub(crate) fn named_create_security_test_registry(
    probe: Arc<NamedCreateSecurityTestProbe>,
) -> FrozenSecurityRegistry {
    const KEY: u64 = 0x6372_6561_7465_0100;

    let mut builder = SecurityRegistryBuilder::try_new()
        .expect("named-create test registry allocation failed")
        .try_register_commoncap()
        .expect("named-create test commoncap registration failed");
    builder
        .try_register_initialized(NamedCreateSecurityTestModule::<KEY> { probe })
        .expect("named-create test module registration failed");
    let registry = Box::new(builder.freeze());
    FrozenSecurityRegistry(Box::leak(registry))
}

#[cfg(test)]
pub(crate) fn named_create_security_test_credential(
    namespace: Arc<UserNamespace>,
    probe: Arc<NamedCreateSecurityTestProbe>,
) -> Arc<Cred> {
    Cred::try_root_with_registry(named_create_security_test_registry(probe), namespace)
        .expect("named-create test credential construction failed")
}

#[cfg(test)]
pub(crate) fn named_create_security_test_unprivileged_credential(
    namespace: Arc<UserNamespace>,
    probe: Arc<NamedCreateSecurityTestProbe>,
    uid: u32,
    gid: u32,
) -> Arc<Cred> {
    let root = named_create_security_test_credential(namespace, probe);
    let slot = crate::task::creds::CredentialSlot::new(root);
    let mut update = slot.prepare();
    let uid =
        crate::task::Kuid::from_raw(uid).expect("named-create test uid must be representable");
    let gid =
        crate::task::Kgid::from_raw(gid).expect("named-create test gid must be representable");
    update.builder.ids = crate::task::Credentials {
        ruid: uid,
        euid: uid,
        suid: uid,
        fsuid: uid,
        rgid: gid,
        egid: gid,
        sgid: gid,
        fsgid: gid,
    };
    let caps = update.builder.caps;
    update.builder.caps = crate::task::creds::capability_state_for_test(
        [0; crate::task::creds::CAPABILITY_WORDS],
        [0; crate::task::creds::CAPABILITY_WORDS],
        [0; crate::task::creds::CAPABILITY_WORDS],
        caps.bounding(),
        [0; crate::task::creds::CAPABILITY_WORDS],
        caps.securebits(),
    );
    update
        .finish()
        .expect("named-create test unprivileged credential preparation failed")
        .commit()
}
