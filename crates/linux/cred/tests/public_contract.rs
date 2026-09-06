use std::{string::String, sync::Arc};

use thekernel_linux_cred::{
    CAPABILITY_WORDS, CapabilityNumber, CapabilitySecurityOperation, CapabilitySets,
    CapsetAuthority, CapsetRequest, ContentWriteMode, ContentWriteSetIdAuthority,
    ContentWriteSetIdCleanup, CredError, Credential, CredentialPublicationContext,
    CredentialPublicationOperation, ExecCredentialInput, ExecDumpability, ExecFileOwner,
    ExecImageReadability, ExecMountPrivilege, ExecTraceState, ExecUserNamespaceView,
    FileMprotectContext, FileOpenAccess, FileOpenContext, FileOpenOperation, FsCredentialSnapshot,
    GroupIdAuthority, GroupIdTransitionInput, IdMap, InodeChmodIntent, InodeChownIntent,
    InodeCreateContext, InodeCreateMode, InodeLinkContext, InodeMkdirContext, InodeMknodContext,
    InodeMknodKind, InodeMknodOperation, InodePermissionAccess, InodePermissionContext,
    InodePostSetattrContext, InodeRenameContext, InodeRmdirContext, InodeSetattrContext,
    InodeSetattrIntent, InodeSetattrMode, InodeSetattrPrivilegeCleanup, InodeSetattrProposal,
    InodeSymlinkContext, InodeTimestampIntent, InodeTimestampValue, InodeUnlinkContext,
    InodeXattrContext, InodeXattrOperation, KeyPermission, KeyPermissionMask, Kgid, Kuid,
    MemoryProtection, MmapAddressContext, MmapFileContext, MmapFileFlags, MmapFileOperation,
    MmapFileSecurityRef, MmapFileTarget, PreparedCredentialCapabilityOperation,
    PtraceAccessContext, PtraceAccessKind, PtraceCredentialKind, PtraceTracemeContext,
    SECBIT_EXEC_DENY_INTERACTIVE, SECBIT_EXEC_DENY_INTERACTIVE_LOCKED, SECBIT_EXEC_RESTRICT_FILE,
    SECBIT_EXEC_RESTRICT_FILE_LOCKED, SECURE_ALL_UNPRIVILEGED, SECURITY_CAPABILITY_XATTR_NAME,
    SchedulerSecurityContext, SchedulerSecurityOperation, SignalCoreAuthorizationReason,
    SignalDeliveryScope, SignalNumber, SignalSecurityContext, SignalSecurityOperation,
    SignalSecuritySource, SocketAcceptContext, SocketBindContext, SocketConnectContext,
    SocketCreateContext, SocketCreateSpec, SocketGetOptionContext, SocketGetPeerNameContext,
    SocketGetSockNameContext, SocketListenBacklog, SocketListenContext, SocketOption,
    SocketPairContext, SocketPostCreateContext, SocketReceiveMessageContext,
    SocketSendMessageContext, SocketSetOptionContext, SocketShutdownContext,
    TaskGetSchedulerContext, UnixMaySendContext, UnixStreamConnectContext, UserIdAuthority,
    UserIdTransitionInput, UserNamespaceDomain, UserNamespaceMapState, UserNamespaceView,
    XATTR_NAME_MAX, XattrSetFlags, XattrValueClass, authorize_capability_core,
    authorize_prepared_credential_capability_core, authorize_signal_core,
    commoncap_exec_transition, commoncap_ptrace_access, commoncap_ptrace_traceme,
    commoncap_scheduler, derive_exec_credential, parse_file_capabilities, plan_capset,
    plan_content_write_setid_cleanup, plan_group_id_transition, plan_user_id_transition,
};

#[test]
fn key_permission_policy_is_public_and_uses_the_frozen_filesystem_identity() {
    let namespace = TestNamespace::initial("key-permission");
    let actor = Credential::try_root(namespace).unwrap();
    let snapshot = actor.fs_credential_snapshot();
    let permissions = KeyPermissionMask::try_from_raw(0x0003_0000).unwrap();
    let requested = KeyPermission::VIEW | KeyPermission::READ;

    assert_eq!(permissions.into_raw(), 0x0003_0000);
    assert!(permissions.allows(
        Kuid::INITIAL_ROOT,
        Some(Kgid::INITIAL_ROOT),
        &snapshot,
        false,
        requested,
    ));
    assert!(!permissions.allows(
        Kuid::from_raw(1000).unwrap(),
        Some(Kgid::from_raw(1000).unwrap()),
        &snapshot,
        false,
        requested,
    ));

    let other_only = KeyPermissionMask::try_from_raw(0x0000_0001).unwrap();
    assert!(other_only.allows(
        Kuid::from_raw(1000).unwrap(),
        None,
        &snapshot,
        false,
        KeyPermission::VIEW,
    ));
}

struct TestNamespace {
    label: &'static str,
    parent: Option<Arc<Self>>,
    level: u32,
    owner: Kuid,
    root: Option<Kuid>,
    uid_map: Arc<IdMap>,
    gid_map: Arc<IdMap>,
}

impl TestNamespace {
    fn initial(label: &'static str) -> Arc<Self> {
        let identity = IdMap::try_identity().unwrap();
        Arc::new(Self {
            label,
            parent: None,
            level: 0,
            owner: Kuid::INITIAL_ROOT,
            root: Some(Kuid::INITIAL_ROOT),
            uid_map: identity.clone(),
            gid_map: identity,
        })
    }

    fn child(parent: &Arc<Self>, label: &'static str, owner: Kuid) -> Arc<Self> {
        Arc::new(Self {
            label,
            parent: Some(parent.clone()),
            level: parent.level + 1,
            owner,
            root: Some(Kuid::INITIAL_ROOT),
            uid_map: parent.uid_map.clone(),
            gid_map: parent.gid_map.clone(),
        })
    }
}

impl UserNamespaceView for TestNamespace {
    fn parent(self: &Arc<Self>) -> Option<Arc<Self>> {
        self.parent.clone()
    }

    fn level(&self) -> u32 {
        self.level
    }

    fn owner_kuid(&self) -> Kuid {
        self.owner
    }

    fn root_kuid(&self) -> Option<Kuid> {
        self.root
    }

    fn is_initial(&self) -> bool {
        self.parent.is_none()
    }
}

impl ExecUserNamespaceView for TestNamespace {
    fn exec_id_map_snapshot(&self) -> (Arc<IdMap>, Arc<IdMap>) {
        (self.uid_map.clone(), self.gid_map.clone())
    }
}

struct NonCopyObject {
    identity: String,
}

struct PreparedSocketAddress {
    bytes: Vec<u8>,
}

struct PreparedSocketMessage {
    identity: String,
    normalized_flags: i32,
}

struct FirstPairSocket(String);
struct SecondPairSocket(String);
struct ListeningSocket(String);
struct AcceptedSocket(String);
struct ConnectingUnixSocket(String);
struct ListeningUnixSocket(String);
struct AcceptedUnixSocket(String);
struct SendingUnixSocket(String);
struct ReceivingUnixSocket(String);
struct MappedFile(String);
struct MemoryImage(String);
struct PreChangeVma {
    identity: String,
    old_protection: MemoryProtection,
}

fn borrow_bind_parts<'a>(
    context: &SocketBindContext<'a, TestNamespace, NonCopyObject, PreparedSocketAddress>,
) -> (
    &'a Credential<TestNamespace>,
    &'a NonCopyObject,
    &'a PreparedSocketAddress,
) {
    (context.actor(), context.socket(), context.address())
}

type UnixStreamTestContext<'a> = UnixStreamConnectContext<
    'a,
    TestNamespace,
    ConnectingUnixSocket,
    ListeningUnixSocket,
    AcceptedUnixSocket,
>;

fn borrow_unix_stream_roles<'a>(
    context: &UnixStreamTestContext<'a>,
) -> (
    &'a ConnectingUnixSocket,
    &'a ListeningUnixSocket,
    &'a AcceptedUnixSocket,
) {
    (
        context.connecting_socket(),
        context.listening_socket(),
        context.accepted_socket(),
    )
}

fn borrow_mmap_file_parts<'a>(
    context: &MmapFileContext<'a, TestNamespace, MappedFile>,
) -> (
    &'a Credential<TestNamespace>,
    &'a Arc<TestNamespace>,
    &'a MappedFile,
) {
    (
        context.actor(),
        context.target().filesystem_owner_user_ns().unwrap(),
        context.target().file_object().unwrap(),
    )
}

fn overridden_dac_credential(
    actor: &Credential<TestNamespace>,
    raw_id: u32,
) -> FsCredentialSnapshot {
    FsCredentialSnapshot::new(
        Kuid::from_raw(raw_id).unwrap(),
        Kgid::from_raw(raw_id).unwrap(),
        actor.groups().clone(),
        [0; CAPABILITY_WORDS],
        false,
    )
}

fn parsed_file_capabilities() -> thekernel_linux_cred::FileCapabilities {
    // Linux VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE, with CAP_CHOWN in
    // the permitted low word and every other capability word empty.
    let record = [
        0x01, 0x00, 0x00, 0x02, // magic_etc
        0x01, 0x00, 0x00, 0x00, // permitted[0]
        0x00, 0x00, 0x00, 0x00, // inheritable[0]
        0x00, 0x00, 0x00, 0x00, // permitted[1]
        0x00, 0x00, 0x00, 0x00, // inheritable[1]
    ];
    parse_file_capabilities(&record).unwrap()
}

#[test]
fn canonical_namespace_and_credential_construction_is_public() {
    let namespace = TestNamespace::initial("initial");
    let domain = UserNamespaceDomain::<TestNamespace>::initial();
    let map_state = UserNamespaceMapState::try_initial().unwrap();

    assert!(domain.is_initial());
    assert_eq!(domain.level(), 0);
    assert_eq!(domain.owner_kuid(), Kuid::INITIAL_ROOT);
    assert!(map_state.uid_map_written());
    assert!(map_state.gid_map_written());
    assert_eq!(
        map_state
            .uid_map()
            .kernel_uid_to_user(Kuid::INITIAL_ROOT)
            .unwrap()
            .into_raw(),
        0
    );

    let root = Credential::try_root(namespace.clone()).unwrap();
    let child_domain = UserNamespaceDomain::try_child(
        &namespace,
        namespace.uid_map.as_ref(),
        namespace.gid_map.as_ref(),
        root.ids().euid,
        root.ids().egid,
        true,
    )
    .unwrap();
    assert_eq!(child_domain.level(), 1);
    assert!(Arc::ptr_eq(
        child_domain.parent().as_ref().unwrap(),
        &namespace
    ));

    let child_namespace = TestNamespace::child(&namespace, "child", root.ids().euid);
    let entered =
        Credential::try_with_user_namespace(root.as_ref(), child_namespace.clone()).unwrap();
    assert!(Arc::ptr_eq(entered.user_ns(), &child_namespace));
    assert_eq!(entered.ids(), root.ids());
    assert_eq!(entered.fs_credential_snapshot().uid(), entered.ids().fsuid);
    assert!(entered.capabilities().has_effective(0));
}

#[test]
fn ordinary_transition_proposal_is_bound_to_the_exact_old_arc() {
    let namespace = TestNamespace::initial("ordinary-transition");
    let old = Credential::try_root(namespace.clone()).unwrap();
    let equal_looking_but_distinct = Credential::try_root(namespace).unwrap();

    let rejected = Credential::try_prepare_transition(
        &old,
        old.ids(),
        old.groups().clone(),
        old.capabilities(),
        true,
    )
    .unwrap();
    assert!(std::ptr::eq(rejected.old(), old.as_ref()));
    assert!(rejected.proposed().no_new_privs());
    assert_eq!(
        rejected
            .try_into_proposed(&equal_looking_but_distinct)
            .err(),
        Some(CredError::NotPermitted)
    );

    let accepted = Credential::try_prepare_transition(
        &old,
        old.ids(),
        old.groups().clone(),
        old.capabilities(),
        true,
    )
    .unwrap();
    assert!(!accepted.effects().requires_dumpability_drop());
    assert!(!accepted.effects().clear_pdeath_signal());
    let proposed = accepted.try_into_proposed(&old).unwrap();
    assert!(proposed.no_new_privs());
    assert!(Arc::ptr_eq(proposed.user_ns(), old.user_ns()));
}

#[test]
fn setid_and_capset_planners_are_publicly_composable_with_prepared_credentials() {
    let namespace = TestNamespace::initial("ordinary-mutation-policy");
    let old = Credential::try_root(namespace).unwrap();
    let uid = Kuid::from_raw(1000).unwrap();
    let gid = Kgid::from_raw(1000).unwrap();

    let mut mutable_copy = CapabilitySets::empty();
    mutable_copy.try_set_keep_caps(true).unwrap();
    assert!(mutable_copy.securebits() != 0);
    mutable_copy.try_set_keep_caps(false).unwrap();
    mutable_copy.try_set_securebits(0).unwrap();
    assert_eq!(
        SECBIT_EXEC_RESTRICT_FILE_LOCKED,
        SECBIT_EXEC_RESTRICT_FILE << 1
    );
    assert_eq!(
        SECBIT_EXEC_DENY_INTERACTIVE_LOCKED,
        SECBIT_EXEC_DENY_INTERACTIVE << 1
    );
    assert_eq!(
        SECURE_ALL_UNPRIVILEGED,
        SECBIT_EXEC_RESTRICT_FILE | SECBIT_EXEC_DENY_INTERACTIVE
    );
    mutable_copy
        .try_set_securebits(
            SECURE_ALL_UNPRIVILEGED
                | SECBIT_EXEC_RESTRICT_FILE_LOCKED
                | SECBIT_EXEC_DENY_INTERACTIVE_LOCKED,
        )
        .unwrap();

    let user_plan = plan_user_id_transition(
        old.as_ref(),
        UserIdTransitionInput::setuid(uid),
        UserIdAuthority::CAP_SETUID,
    )
    .unwrap();
    assert!(std::ptr::eq(user_plan.old(), old.as_ref()));
    assert_eq!(user_plan.input(), UserIdTransitionInput::setuid(uid));
    assert_eq!(user_plan.authority(), UserIdAuthority::CAP_SETUID);
    assert_eq!(user_plan.previous_fsuid(), Kuid::INITIAL_ROOT);
    assert_eq!(user_plan.ids().euid, uid);
    assert!(user_plan.changes_credential());

    let prepared = user_plan.try_prepare_credential(&old).unwrap();
    let user = prepared.try_into_proposed(&old).unwrap();
    assert_eq!(user.ids().euid, uid);

    let group_plan = plan_group_id_transition(
        user.as_ref(),
        GroupIdTransitionInput::setgid(gid),
        GroupIdAuthority::CAP_SETGID,
    )
    .unwrap();
    assert!(std::ptr::eq(group_plan.old(), user.as_ref()));
    assert_eq!(group_plan.input(), GroupIdTransitionInput::setgid(gid));
    assert_eq!(group_plan.authority(), GroupIdAuthority::CAP_SETGID);
    assert_eq!(group_plan.ids().egid, gid);
    assert_eq!(group_plan.previous_fsgid(), Kgid::INITIAL_ROOT);
    assert_eq!(group_plan.capabilities(), user.capabilities());
    let group = group_plan
        .try_prepare_credential(&user)
        .unwrap()
        .try_into_proposed(&user)
        .unwrap();
    assert_eq!(group.ids().egid, gid);

    let request = CapsetRequest::try_new(
        [0; CAPABILITY_WORDS],
        [0; CAPABILITY_WORDS],
        [0; CAPABILITY_WORDS],
    )
    .unwrap();
    let capset_plan = plan_capset(group.as_ref(), request, CapsetAuthority::RESTRICTED).unwrap();
    assert!(std::ptr::eq(capset_plan.old(), group.as_ref()));
    assert_eq!(capset_plan.request(), request);
    assert_eq!(capset_plan.authority(), CapsetAuthority::RESTRICTED);
    assert_eq!(request.effective(), [0; CAPABILITY_WORDS]);
    assert_eq!(request.permitted(), [0; CAPABILITY_WORDS]);
    assert_eq!(request.inheritable(), [0; CAPABILITY_WORDS]);
    assert_eq!(capset_plan.ids(), group.ids());
    assert_eq!(
        capset_plan.capabilities().effective(),
        [0; CAPABILITY_WORDS]
    );
    assert_eq!(
        capset_plan.capabilities().permitted(),
        [0; CAPABILITY_WORDS]
    );
    let capped = capset_plan
        .try_prepare_credential(&group)
        .unwrap()
        .try_into_proposed(&group)
        .unwrap();
    assert_eq!(capped.ids(), group.ids());
}

#[test]
fn content_write_setid_planner_exposes_checked_input_effect_and_next_mode() {
    assert_eq!(ContentWriteMode::try_from_bits(0o100000 | 0o6755), None);
    let mode = ContentWriteMode::try_from_bits(0o6755).unwrap();

    let privileged = plan_content_write_setid_cleanup(mode, ContentWriteSetIdAuthority::CAP_FSETID);
    assert_eq!(privileged.mode(), mode);
    assert_eq!(
        privileged.authority(),
        ContentWriteSetIdAuthority::CAP_FSETID
    );
    assert_eq!(privileged.cleanup(), ContentWriteSetIdCleanup::Preserve);
    assert_eq!(privileged.next_mode(), mode);
    assert!(!privileged.changes_mode());

    let cases = [
        (
            0o6755,
            ContentWriteSetIdCleanup::ClearSetUserAndGroupId,
            0o0755,
        ),
        (0o4644, ContentWriteSetIdCleanup::ClearSetUserId, 0o0644),
        (0o2755, ContentWriteSetIdCleanup::ClearSetGroupId, 0o0755),
        (0o2644, ContentWriteSetIdCleanup::Preserve, 0o2644),
        (0o0755, ContentWriteSetIdCleanup::Preserve, 0o0755),
    ];
    for (before, cleanup, after) in cases {
        let plan = plan_content_write_setid_cleanup(
            ContentWriteMode::try_from_bits(before).unwrap(),
            ContentWriteSetIdAuthority::UNPRIVILEGED,
        );
        assert_eq!(plan.cleanup(), cleanup);
        assert_eq!(plan.next_mode().bits(), after);
        assert_eq!(plan.changes_mode(), before != after);
    }
}

#[test]
fn file_capability_parser_and_exec_proposal_are_publicly_composable() {
    assert_eq!(SECURITY_CAPABILITY_XATTR_NAME, b"security.capability");
    let file_capabilities = parsed_file_capabilities();
    assert_eq!(file_capabilities.permitted()[0], 1);
    assert_eq!(file_capabilities.inheritable(), [0; CAPABILITY_WORDS]);
    assert!(file_capabilities.effective());
    assert_eq!(file_capabilities.rootid(), Kuid::INITIAL_ROOT);

    let namespace = TestNamespace::initial("exec");
    let old = Credential::try_root(namespace).unwrap();
    let input = ExecCredentialInput::new(
        0o755,
        Some(ExecFileOwner::new(Kuid::INITIAL_ROOT, Kgid::INITIAL_ROOT)),
        ExecMountPrivilege::Honor,
        ExecTraceState::NotSuppressingPrivilege,
        ExecImageReadability::Readable,
        Some(file_capabilities),
    );
    let proposal = derive_exec_credential(&old, input).unwrap();

    assert!(std::ptr::eq(proposal.old(), old.as_ref()));
    assert_eq!(proposal.input(), input);
    assert_eq!(
        proposal.effects().dumpability(),
        ExecDumpability::UserDumpable
    );
    assert!(!proposal.effects().secure_exec());
    assert!(!proposal.revalidation().privilege_sensitive());
    commoncap_exec_transition(&proposal).unwrap();

    let proposed = proposal.try_into_proposed(&old).unwrap();
    assert!(Arc::ptr_eq(proposed.user_ns(), old.user_ns()));
}

#[test]
fn ptrace_scheduler_and_signal_contexts_compile_from_root_exports() {
    let namespace = TestNamespace::initial("security-contexts");
    let actor = Credential::try_root(namespace.clone()).unwrap();
    let target = Credential::try_root(namespace.clone()).unwrap();
    let object = NonCopyObject {
        identity: String::from("exact-security-target"),
    };

    let ptrace = PtraceAccessContext::new(
        &actor,
        &target,
        &namespace,
        &object,
        PtraceAccessKind::Read,
        PtraceCredentialKind::Real,
    );
    commoncap_ptrace_access(&ptrace).unwrap();
    assert!(std::ptr::eq(ptrace.actor(), actor.as_ref()));
    assert!(std::ptr::eq(ptrace.target(), target.as_ref()));
    assert!(Arc::ptr_eq(ptrace.target_image_owner_user_ns(), &namespace));
    assert_eq!(ptrace.target_object().identity, "exact-security-target");

    let traceme = PtraceTracemeContext::new(&actor, &target, &namespace, &object);
    commoncap_ptrace_traceme(&traceme).unwrap();
    assert!(std::ptr::eq(traceme.parent_actor(), actor.as_ref()));
    assert!(std::ptr::eq(traceme.child_target(), target.as_ref()));

    let scheduler =
        SchedulerSecurityContext::new(&actor, &target, SchedulerSecurityOperation::SetAffinity);
    commoncap_scheduler(&scheduler).unwrap();
    assert!(scheduler.owner_match());
    assert_eq!(
        scheduler.operation(),
        SchedulerSecurityOperation::SetAffinity
    );

    let scheduler_query = TaskGetSchedulerContext::new(&actor, &target);
    assert!(std::ptr::eq(scheduler_query.actor(), actor.as_ref()));
    assert!(std::ptr::eq(scheduler_query.target(), target.as_ref()));

    let signal_operation = SignalSecurityOperation::send(
        SignalNumber::try_new(15).unwrap(),
        SignalSecuritySource::Kill,
        SignalDeliveryScope::ThreadGroup,
    );
    let authorization =
        authorize_signal_core(&actor, &target, signal_operation, false, false).unwrap();
    assert_eq!(
        authorization.reason(),
        SignalCoreAuthorizationReason::CredentialMatch
    );
    let signal = SignalSecurityContext::new(authorization, &object);
    assert!(std::ptr::eq(signal.actor(), actor.as_ref()));
    assert!(std::ptr::eq(signal.target(), target.as_ref()));
    assert!(Arc::ptr_eq(signal.target_owner_user_ns(), &namespace));
    assert_eq!(signal.target_object().identity, "exact-security-target");
    assert_eq!(signal.operation(), signal_operation);
}

#[test]
fn capable_and_credential_publication_contracts_are_publicly_composable() {
    let root_namespace = TestNamespace::initial("capable-root");
    let root = Credential::try_root(root_namespace.clone()).unwrap();
    let capability = CapabilityNumber::try_new(0).unwrap();
    assert_eq!(capability.get(), 0);
    assert!(CapabilityNumber::try_new(CapabilityNumber::MAX).is_some());
    assert_eq!(CapabilityNumber::try_new(CapabilityNumber::MAX + 1), None);

    let capable = authorize_capability_core(
        &root,
        &root_namespace,
        capability,
        CapabilitySecurityOperation::UseWithoutAudit,
    )
    .unwrap();
    assert!(std::ptr::eq(capable.actor(), root.as_ref()));
    assert!(Arc::ptr_eq(capable.target_user_ns(), &root_namespace));
    assert_eq!(capable.capability(), capability);
    assert_eq!(
        capable.operation(),
        CapabilitySecurityOperation::UseWithoutAudit
    );

    let child_namespace = TestNamespace::child(&root_namespace, "capable-child", root.ids().euid);
    let child = Credential::try_with_user_namespace(&root, child_namespace.clone()).unwrap();
    let prepared = authorize_prepared_credential_capability_core(
        &root,
        &child,
        &child_namespace,
        capability,
        PreparedCredentialCapabilityOperation::NamespaceCreate,
    )
    .unwrap();
    assert!(std::ptr::eq(prepared.source_credential(), root.as_ref()));
    assert!(std::ptr::eq(prepared.proposed_credential(), child.as_ref()));
    assert!(Arc::ptr_eq(prepared.target_user_ns(), &child_namespace));
    assert_eq!(prepared.capability(), capability);
    assert_eq!(
        prepared.operation(),
        PreparedCredentialCapabilityOperation::NamespaceCreate
    );
    assert_eq!(
        authorize_capability_core(
            &child,
            &root_namespace,
            capability,
            CapabilitySecurityOperation::Use,
        )
        .err(),
        Some(thekernel_linux_cred::AuthorizationError::NotPermitted)
    );

    let fork_target = NonCopyObject {
        identity: String::from("published-fork-child"),
    };
    let fork = CredentialPublicationContext::fork(&root, &root, &fork_target);
    assert!(std::ptr::eq(fork.source_credential(), root.as_ref()));
    assert!(std::ptr::eq(fork.published_credential(), root.as_ref()));
    assert!(Arc::ptr_eq(fork.source_user_ns(), &root_namespace));
    assert!(Arc::ptr_eq(fork.target_user_ns(), &root_namespace));
    assert!(std::ptr::eq(fork.target_object(), &fork_target));
    assert_eq!(fork.operation(), CredentialPublicationOperation::Fork);

    let userns_target = NonCopyObject {
        identity: String::from("published-userns-child"),
    };
    let userns = CredentialPublicationContext::user_namespace(&root, &child, &userns_target);
    assert!(std::ptr::eq(userns.source_credential(), root.as_ref()));
    assert!(std::ptr::eq(userns.published_credential(), child.as_ref()));
    assert!(Arc::ptr_eq(userns.source_user_ns(), &root_namespace));
    assert!(Arc::ptr_eq(userns.target_user_ns(), &child_namespace));
    assert!(std::ptr::eq(userns.target_object(), &userns_target));
    assert_eq!(
        userns.operation(),
        CredentialPublicationOperation::UserNamespace
    );
}

#[test]
fn inode_permission_public_contract_binds_distinct_frozen_inputs() {
    let actor_namespace = TestNamespace::initial("actor");
    let target_owner_namespace = TestNamespace::initial("target-owner");
    let actor = Credential::try_root(actor_namespace.clone()).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 1000);
    let object = NonCopyObject {
        identity: String::from("exact-inode-location"),
    };
    let access =
        InodePermissionAccess::READ | InodePermissionAccess::WRITE | InodePermissionAccess::EXECUTE;
    let context = InodePermissionContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &object,
        access,
    );

    assert!(std::ptr::eq(context.actor(), actor.as_ref()));
    assert!(std::ptr::eq(context.dac_credential(), &dac_credential));
    assert_ne!(context.dac_credential().uid(), actor.ids().fsuid);
    assert!(Arc::ptr_eq(
        context.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(!Arc::ptr_eq(
        context.target_owner_user_ns(),
        actor.user_ns()
    ));
    assert_eq!(context.target_owner_user_ns().label, "target-owner");
    assert!(std::ptr::eq(context.target_object(), &object));
    assert_eq!(context.target_object().identity, "exact-inode-location");
    assert_eq!(context.access(), InodePermissionAccess::ALL);
    assert!(context.access().contains(InodePermissionAccess::WRITE));
    assert_eq!(InodePermissionAccess::try_from_bits(0), None);
    assert_eq!(
        InodePermissionAccess::try_from_bits(InodePermissionAccess::ALL.bits() | (1 << 7)),
        None
    );
}

#[test]
fn inode_setattr_public_contract_preserves_omission_and_hook_point_effects() {
    let actor_namespace = TestNamespace::initial("setattr-actor");
    let target_owner_namespace = TestNamespace::initial("setattr-owner");
    let actor = Credential::try_root(actor_namespace).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 2400);
    let old_object = NonCopyObject {
        identity: String::from("exact-old-setattr-inode"),
    };
    let committed_object = NonCopyObject {
        identity: String::from("exact-committed-setattr-inode"),
    };

    assert_eq!(InodeSetattrMode::try_from_bits(0).unwrap().bits(), 0);
    assert_eq!(
        InodeSetattrMode::try_from_bits(0o7777).unwrap().bits(),
        0o7777
    );
    assert_eq!(InodeSetattrMode::try_from_bits(0o100000 | 0o644), None);

    let chmod_intent = InodeChmodIntent::new(InodeSetattrMode::try_from_bits(0o2750).unwrap());
    let chmod = InodeSetattrProposal::chmod(chmod_intent);
    assert_eq!(chmod.intent(), InodeSetattrIntent::Chmod(chmod_intent));
    assert_eq!(chmod.mode(), Some(chmod_intent.mode()));
    assert_eq!(chmod.user(), None);
    assert_eq!(chmod.group(), None);
    assert_eq!(
        chmod.privilege_cleanup(),
        InodeSetattrPrivilegeCleanup::Preserve
    );

    let timestamps =
        InodeTimestampIntent::new(InodeTimestampValue::Explicit, InodeTimestampValue::Now);
    let timestamp_proposal = InodeSetattrProposal::timestamps(timestamps);
    assert_eq!(timestamps.atime(), InodeTimestampValue::Explicit);
    assert_eq!(timestamps.mtime(), InodeTimestampValue::Now);
    assert_eq!(
        timestamp_proposal.intent(),
        InodeSetattrIntent::Timestamp(timestamps)
    );
    assert_eq!(timestamp_proposal.mode(), None);
    assert_eq!(timestamp_proposal.user(), None);
    assert_eq!(timestamp_proposal.group(), None);

    let omitted = InodeChownIntent::new(None, None);
    let omitted_proposal = InodeSetattrProposal::chown(
        omitted,
        Some(InodeSetattrMode::try_from_bits(0o755).unwrap()),
        InodeSetattrPrivilegeCleanup::Kill,
    );
    assert_eq!(
        omitted_proposal.intent(),
        InodeSetattrIntent::Chown(omitted)
    );
    assert_eq!(omitted_proposal.user(), None);
    assert_eq!(omitted_proposal.group(), None);
    assert_eq!(
        omitted_proposal.privilege_cleanup(),
        InodeSetattrPrivilegeCleanup::Kill
    );

    let requested_user = Kuid::from_raw(2401).unwrap();
    let requested_group = Kgid::from_raw(2402).unwrap();
    let explicit = InodeChownIntent::new(Some(requested_user), Some(requested_group));
    let explicit_proposal =
        InodeSetattrProposal::chown(explicit, None, InodeSetattrPrivilegeCleanup::Preserve);
    assert_ne!(explicit, omitted);
    assert_eq!(explicit.user(), Some(requested_user));
    assert_eq!(explicit.group(), Some(requested_group));
    assert_eq!(explicit_proposal.user(), Some(requested_user));
    assert_eq!(explicit_proposal.group(), Some(requested_group));

    let pre = InodeSetattrContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &old_object,
        omitted_proposal,
    );
    assert!(std::ptr::eq(pre.actor(), actor.as_ref()));
    assert!(std::ptr::eq(pre.dac_credential(), &dac_credential));
    assert_ne!(pre.dac_credential().uid(), actor.ids().fsuid);
    assert!(Arc::ptr_eq(
        pre.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(pre.target_object(), &old_object));
    assert_eq!(pre.target_object().identity, "exact-old-setattr-inode");
    assert_eq!(pre.proposal(), omitted_proposal);
    assert_eq!(pre.intent(), InodeSetattrIntent::Chown(omitted));

    let post = InodePostSetattrContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &committed_object,
        omitted_proposal,
    );
    assert!(std::ptr::eq(post.actor(), actor.as_ref()));
    assert!(std::ptr::eq(post.dac_credential(), &dac_credential));
    assert!(Arc::ptr_eq(
        post.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(post.committed_object(), &committed_object));
    assert_eq!(
        post.committed_object().identity,
        "exact-committed-setattr-inode"
    );
    assert_eq!(post.proposal(), omitted_proposal);
    assert_eq!(post.intent(), InodeSetattrIntent::Chown(omitted));
}

#[test]
fn named_inode_creation_public_contract_preserves_linux_hook_roles() {
    let actor_namespace = TestNamespace::initial("actor");
    let target_owner_namespace = TestNamespace::initial("named-create-owner");
    let actor = Credential::try_root(actor_namespace).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 3000);
    let parent = NonCopyObject {
        identity: String::from("exact-parent-directory"),
    };
    let new_entry = NonCopyObject {
        identity: String::from("exact-prospective-named-entry"),
    };

    assert_eq!(InodeCreateMode::try_from_bits(0).unwrap().bits(), 0);
    assert_eq!(
        InodeCreateMode::try_from_bits(0o7777).unwrap().bits(),
        0o7777
    );
    assert_eq!(InodeCreateMode::try_from_bits(0o100000 | 0o644), None);

    let file_mode = InodeCreateMode::try_from_bits(0o640).unwrap();
    let create = InodeCreateContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &parent,
        &new_entry,
        file_mode,
    );
    assert!(std::ptr::eq(create.actor(), actor.as_ref()));
    assert!(std::ptr::eq(create.dac_credential(), &dac_credential));
    assert!(Arc::ptr_eq(
        create.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(create.parent_object(), &parent));
    assert!(std::ptr::eq(create.new_entry_object(), &new_entry));
    assert_eq!(create.parent_object().identity, "exact-parent-directory");
    assert_eq!(
        create.new_entry_object().identity,
        "exact-prospective-named-entry"
    );
    assert_eq!(create.mode(), file_mode);

    let directory_mode = InodeCreateMode::try_from_bits(0o2750).unwrap();
    let mkdir = InodeMkdirContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &parent,
        &new_entry,
        directory_mode,
    );
    assert!(std::ptr::eq(mkdir.parent_object(), &parent));
    assert!(std::ptr::eq(mkdir.new_entry_object(), &new_entry));
    assert_eq!(mkdir.mode(), directory_mode);

    let special_mode = InodeCreateMode::try_from_bits(0o620).unwrap();
    assert_eq!(
        InodeMknodOperation::new(InodeMknodKind::Fifo, special_mode, Some(1)),
        None
    );
    assert_eq!(
        InodeMknodOperation::new(InodeMknodKind::CharacterDevice, special_mode, None),
        None
    );
    let operation =
        InodeMknodOperation::new(InodeMknodKind::CharacterDevice, special_mode, Some(0x0501))
            .unwrap();
    let mknod = InodeMknodContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &parent,
        &new_entry,
        operation,
    );
    assert!(std::ptr::eq(mknod.parent_object(), &parent));
    assert!(std::ptr::eq(mknod.new_entry_object(), &new_entry));
    assert_eq!(mknod.operation().kind(), InodeMknodKind::CharacterDevice);
    assert_eq!(mknod.operation().mode(), special_mode);
    assert_eq!(mknod.operation().rdev(), Some(0x0501));

    assert!(InodeMknodOperation::new(InodeMknodKind::Fifo, special_mode, None).is_some());
    assert!(InodeMknodOperation::new(InodeMknodKind::Socket, special_mode, None).is_some());
    assert!(InodeMknodOperation::new(InodeMknodKind::BlockDevice, special_mode, Some(0)).is_some());

    let target = vec![0xff, b'/', b't', b'a', b'r', b'g', b'e', b't'];
    let symlink = InodeSymlinkContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &parent,
        &new_entry,
        target.as_slice(),
    );
    assert!(std::ptr::eq(symlink.actor(), actor.as_ref()));
    assert!(std::ptr::eq(symlink.dac_credential(), &dac_credential));
    assert!(Arc::ptr_eq(
        symlink.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(symlink.parent_object(), &parent));
    assert!(std::ptr::eq(symlink.new_entry_object(), &new_entry));
    assert!(std::ptr::eq(symlink.symlink_target(), target.as_slice()));
    assert_eq!(symlink.symlink_target(), target.as_slice());
}

#[test]
fn inode_link_public_contract_preserves_source_and_destination_roles() {
    let actor_namespace = TestNamespace::initial("actor");
    let target_owner_namespace = TestNamespace::initial("hard-link-owner");
    let actor = Credential::try_root(actor_namespace).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 3100);
    let source = NonCopyObject {
        identity: String::from("exact-source-inode"),
    };
    let parent = NonCopyObject {
        identity: String::from("exact-destination-parent"),
    };
    let new_entry = NonCopyObject {
        identity: String::from("exact-prospective-hard-link-entry"),
    };
    let context = InodeLinkContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &source,
        &parent,
        &new_entry,
    );

    assert!(std::ptr::eq(context.actor(), actor.as_ref()));
    assert!(std::ptr::eq(context.dac_credential(), &dac_credential));
    assert_ne!(context.dac_credential().uid(), actor.ids().fsuid);
    assert!(Arc::ptr_eq(
        context.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(context.source_object(), &source));
    assert!(std::ptr::eq(context.parent_object(), &parent));
    assert!(std::ptr::eq(context.new_entry_object(), &new_entry));
    assert_eq!(context.source_object().identity, "exact-source-inode");
    assert_eq!(context.parent_object().identity, "exact-destination-parent");
    assert_eq!(
        context.new_entry_object().identity,
        "exact-prospective-hard-link-entry"
    );
}

#[test]
fn inode_removal_public_contracts_preserve_parent_and_existing_entry_roles() {
    let actor_namespace = TestNamespace::initial("actor");
    let target_owner_namespace = TestNamespace::initial("removal-owner");
    let actor = Credential::try_root(actor_namespace).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 3200);
    let parent = NonCopyObject {
        identity: String::from("exact-removal-parent"),
    };
    let target_entry = NonCopyObject {
        identity: String::from("exact-existing-victim-entry"),
    };

    let unlink = InodeUnlinkContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &parent,
        &target_entry,
    );
    assert!(std::ptr::eq(unlink.actor(), actor.as_ref()));
    assert!(std::ptr::eq(unlink.dac_credential(), &dac_credential));
    assert_ne!(unlink.dac_credential().uid(), actor.ids().fsuid);
    assert!(Arc::ptr_eq(
        unlink.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(unlink.parent_object(), &parent));
    assert!(std::ptr::eq(unlink.target_entry_object(), &target_entry));

    let rmdir = InodeRmdirContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &parent,
        &target_entry,
    );
    assert!(std::ptr::eq(rmdir.actor(), actor.as_ref()));
    assert!(std::ptr::eq(rmdir.dac_credential(), &dac_credential));
    assert!(Arc::ptr_eq(
        rmdir.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(rmdir.parent_object(), &parent));
    assert!(std::ptr::eq(rmdir.target_entry_object(), &target_entry));
    assert_eq!(
        rmdir.target_entry_object().identity,
        "exact-existing-victim-entry"
    );
}

#[test]
fn inode_rename_public_contract_preserves_four_ordered_object_roles() {
    let actor_namespace = TestNamespace::initial("actor");
    let target_owner_namespace = TestNamespace::initial("rename-owner");
    let actor = Credential::try_root(actor_namespace).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 3300);
    let old_parent = NonCopyObject {
        identity: String::from("exact-old-parent"),
    };
    let old_entry = NonCopyObject {
        identity: String::from("exact-old-entry-and-source"),
    };
    let new_parent = NonCopyObject {
        identity: String::from("exact-new-parent"),
    };
    let new_entry = NonCopyObject {
        identity: String::from("exact-new-entry-and-target-state"),
    };

    let forward = InodeRenameContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &old_parent,
        &old_entry,
        &new_parent,
        &new_entry,
    );
    assert!(std::ptr::eq(forward.actor(), actor.as_ref()));
    assert!(std::ptr::eq(forward.dac_credential(), &dac_credential));
    assert_ne!(forward.dac_credential().uid(), actor.ids().fsuid);
    assert!(Arc::ptr_eq(
        forward.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(forward.old_parent_object(), &old_parent));
    assert!(std::ptr::eq(forward.old_entry_object(), &old_entry));
    assert!(std::ptr::eq(forward.new_parent_object(), &new_parent));
    assert!(std::ptr::eq(forward.new_entry_object(), &new_entry));
    assert_eq!(forward.old_parent_object().identity, "exact-old-parent");
    assert_eq!(
        forward.old_entry_object().identity,
        "exact-old-entry-and-source"
    );
    assert_eq!(forward.new_parent_object().identity, "exact-new-parent");
    assert_eq!(
        forward.new_entry_object().identity,
        "exact-new-entry-and-target-state"
    );

    // An exchange adapter constructs and dispatches this reverse direction
    // before dispatching `forward`; the leaf payload itself has no flags.
    let reverse = InodeRenameContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &new_parent,
        &new_entry,
        &old_parent,
        &old_entry,
    );
    assert!(std::ptr::eq(reverse.old_parent_object(), &new_parent));
    assert!(std::ptr::eq(reverse.old_entry_object(), &new_entry));
    assert!(std::ptr::eq(reverse.new_parent_object(), &old_parent));
    assert!(std::ptr::eq(reverse.new_entry_object(), &old_entry));
}

#[test]
fn inode_xattr_public_contract_validates_operations_and_preserves_roles() {
    assert_eq!(XattrSetFlags::try_from_bits(0), Some(XattrSetFlags::NONE));
    assert_eq!(
        XattrSetFlags::try_from_bits(0x1),
        Some(XattrSetFlags::CREATE)
    );
    assert_eq!(
        XattrSetFlags::try_from_bits(0x2),
        Some(XattrSetFlags::REPLACE)
    );
    assert_eq!(
        XattrSetFlags::CREATE.bits() | XattrSetFlags::REPLACE.bits(),
        0x3
    );
    assert_eq!(
        XattrSetFlags::try_from_bits(0x3),
        Some(XattrSetFlags::CREATE_AND_REPLACE)
    );
    assert_eq!(XattrSetFlags::try_from_bits(0x4), None);
    assert_eq!(XattrSetFlags::try_from_bits(u32::MAX), None);

    assert_eq!(XATTR_NAME_MAX, 255);
    assert_eq!(InodeXattrOperation::get(b""), None);
    assert_eq!(
        InodeXattrOperation::set(b"", b"value", XattrSetFlags::NONE),
        None
    );
    assert_eq!(InodeXattrOperation::remove(b""), None);
    assert_eq!(InodeXattrOperation::get(b"user.\0hidden"), None);
    assert_eq!(
        InodeXattrOperation::set(b"user.\0hidden", b"value", XattrSetFlags::NONE),
        None
    );
    assert_eq!(InodeXattrOperation::remove(b"user.\0hidden"), None);

    let maximum_name = [0xff; XATTR_NAME_MAX];
    let get = InodeXattrOperation::get(maximum_name.as_slice()).unwrap();
    assert!(std::ptr::eq(get.name().unwrap(), maximum_name.as_slice()));
    assert!(
        InodeXattrOperation::set(maximum_name.as_slice(), b"value", XattrSetFlags::NONE).is_some()
    );
    assert!(InodeXattrOperation::remove(maximum_name.as_slice()).is_some());

    let oversized_name = [b'x'; XATTR_NAME_MAX + 1];
    assert_eq!(InodeXattrOperation::get(oversized_name.as_slice()), None);
    assert_eq!(
        InodeXattrOperation::set(oversized_name.as_slice(), b"value", XattrSetFlags::NONE),
        None
    );
    assert_eq!(InodeXattrOperation::remove(oversized_name.as_slice()), None);

    let get = InodeXattrOperation::get(b"user.example").unwrap();
    assert!(matches!(
        get,
        InodeXattrOperation::Get { name, .. } if name == b"user.example"
    ));
    assert_eq!(get.name(), Some(b"user.example".as_slice()));
    assert_eq!(get.value(), None);
    assert_eq!(get.set_flags(), None);
    assert_eq!(get.value_class(), None);

    let list = InodeXattrOperation::list();
    assert!(matches!(list, InodeXattrOperation::List));
    assert_eq!(list.name(), None);
    assert_eq!(list.value(), None);

    let remove = InodeXattrOperation::remove(b"trusted.example").unwrap();
    assert!(matches!(
        remove,
        InodeXattrOperation::Remove { name, .. } if name == b"trusted.example"
    ));
    assert_eq!(remove.name(), Some(b"trusted.example".as_slice()));
    assert_eq!(remove.value(), None);

    let ordinary_value = [0xff, 0x00];
    let mut non_utf8_name = b"user.".to_vec();
    non_utf8_name.push(0xff);
    assert!(std::str::from_utf8(non_utf8_name.as_slice()).is_err());
    let ordinary = InodeXattrOperation::set(
        non_utf8_name.as_slice(),
        ordinary_value.as_slice(),
        XattrSetFlags::CREATE,
    )
    .unwrap();
    assert!(std::ptr::eq(
        ordinary.name().unwrap(),
        non_utf8_name.as_slice()
    ));
    assert_eq!(ordinary.value_class(), Some(XattrValueClass::Opaque));

    let near_capability = b"security.capability\xff";
    let ordinary = InodeXattrOperation::set(near_capability, &[], XattrSetFlags::NONE).unwrap();
    assert_eq!(ordinary.value_class(), Some(XattrValueClass::Opaque));

    let actor_namespace = TestNamespace::initial("xattr-actor");
    let target_owner_namespace = TestNamespace::initial("xattr-owner");
    let actor = Credential::try_root(actor_namespace).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 3400);
    let target = NonCopyObject {
        identity: String::from("exact-xattr-target"),
    };
    let name = SECURITY_CAPABILITY_XATTR_NAME.to_vec();
    let value = vec![0x01, 0x00, 0x00, 0x02];
    let operation =
        InodeXattrOperation::set(name.as_slice(), value.as_slice(), XattrSetFlags::REPLACE)
            .unwrap();
    assert!(matches!(
        operation,
        InodeXattrOperation::Set {
            flags: XattrSetFlags::REPLACE,
            value_class: XattrValueClass::SecurityCapability,
            ..
        }
    ));

    let context = InodeXattrContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &target,
        operation,
    );
    assert!(std::ptr::eq(context.actor(), actor.as_ref()));
    assert!(std::ptr::eq(context.dac_credential(), &dac_credential));
    assert_ne!(context.dac_credential().uid(), actor.ids().fsuid);
    assert!(Arc::ptr_eq(
        context.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(context.target_object(), &target));
    assert_eq!(context.target_object().identity, "exact-xattr-target");
    assert!(std::ptr::eq(
        context.operation().name().unwrap(),
        name.as_slice()
    ));
    assert!(std::ptr::eq(
        context.operation().value().unwrap(),
        value.as_slice()
    ));
    assert_eq!(
        context.operation().set_flags(),
        Some(XattrSetFlags::REPLACE)
    );
    assert_eq!(
        context.operation().value_class(),
        Some(XattrValueClass::SecurityCapability)
    );
}

#[test]
fn file_open_public_contract_normalizes_flags_without_raw_fd_values() {
    let actor_namespace = TestNamespace::initial("actor");
    let target_owner_namespace = TestNamespace::initial("open-owner");
    let actor = Credential::try_root(actor_namespace).unwrap();
    let dac_credential = overridden_dac_credential(&actor, 2000);
    let object = NonCopyObject {
        identity: String::from("exact-open-location"),
    };
    let operation =
        FileOpenOperation::new(FileOpenAccess::ReadWrite, true, true, true, false).unwrap();
    let context = FileOpenContext::new(
        &actor,
        &dac_credential,
        &target_owner_namespace,
        &object,
        operation,
    );

    assert!(std::ptr::eq(context.actor(), actor.as_ref()));
    assert!(std::ptr::eq(context.dac_credential(), &dac_credential));
    assert_ne!(context.dac_credential().uid(), actor.ids().fsuid);
    assert!(Arc::ptr_eq(
        context.target_owner_user_ns(),
        &target_owner_namespace
    ));
    assert!(std::ptr::eq(context.target_object(), &object));
    assert_eq!(context.target_object().identity, "exact-open-location");
    assert_eq!(context.operation(), operation);
    assert_eq!(operation.access(), FileOpenAccess::ReadWrite);
    assert!(operation.access().reads());
    assert!(operation.access().writes());
    assert!(operation.append());
    assert!(operation.truncate());
    assert!(operation.created());
    assert!(!operation.unnamed());

    let unnamed = FileOpenOperation::new(FileOpenAccess::Write, false, false, true, true)
        .expect("O_TMPFILE is created, unnamed, and writable");
    assert!(unnamed.created());
    assert!(unnamed.unnamed());
    assert_eq!(
        FileOpenOperation::new(FileOpenAccess::Write, false, false, false, true),
        None
    );

    let read_truncate = FileOpenOperation::new(FileOpenAccess::Read, false, true, false, false)
        .expect("Linux O_RDONLY|O_TRUNC remains a representable normalized request");
    assert_eq!(read_truncate.access(), FileOpenAccess::Read);
    assert!(read_truncate.truncate());

    let no_data = FileOpenOperation::new(FileOpenAccess::NoData, false, true, true, false)
        .expect("Linux access mode 3 may retain open-time truncate/create facts");
    assert_eq!(no_data.access(), FileOpenAccess::NoData);
    assert!(!no_data.access().reads());
    assert!(!no_data.access().writes());
    assert!(no_data.truncate());
    assert!(no_data.created());
    assert_eq!(
        FileOpenOperation::new(FileOpenAccess::NoData, true, false, false, false),
        None
    );
    let no_data_unnamed = FileOpenOperation::new(FileOpenAccess::NoData, false, false, true, true)
        .expect("mode-3 MAY_WRITE admission permits O_TMPFILE creation");
    assert!(!no_data_unnamed.access().writes());
    assert!(no_data_unnamed.created());
    assert!(no_data_unnamed.unnamed());
    assert_eq!(
        FileOpenOperation::new(FileOpenAccess::NoData, false, false, false, true),
        None
    );
}

#[test]
fn socket_create_pair_listen_and_accept_contracts_preserve_leaf_roles() {
    let namespace = TestNamespace::initial("socket-create");
    let actor = Credential::try_root(namespace).unwrap();

    assert_eq!(SocketCreateSpec::try_new(2, -1, 6, false), None);
    assert_eq!(SocketCreateSpec::try_new(2, 1 | 0x800, 6, false), None);
    let spec = SocketCreateSpec::try_new(2, 1, 6, false).unwrap();
    assert_eq!(spec.family(), 2);
    assert_eq!(spec.socket_type(), 1);
    assert_eq!(spec.protocol(), 6);
    assert!(!spec.kernel_origin());

    let create = SocketCreateContext::new(&actor, spec);
    assert!(std::ptr::eq(create.actor(), actor.as_ref()));
    assert_eq!(create.spec(), spec);

    let created_socket = NonCopyObject {
        identity: String::from("post-create-socket"),
    };
    let post_create = SocketPostCreateContext::new(&actor, &created_socket, spec);
    assert!(std::ptr::eq(post_create.actor(), actor.as_ref()));
    assert!(std::ptr::eq(post_create.created_socket(), &created_socket));
    assert_eq!(post_create.created_socket().identity, "post-create-socket");
    assert_eq!(post_create.spec(), create.spec());

    let first = FirstPairSocket(String::from("first-pair-endpoint"));
    let second = SecondPairSocket(String::from("second-pair-endpoint"));
    let pair = SocketPairContext::new(&actor, &first, &second);
    assert!(std::ptr::eq(pair.actor(), actor.as_ref()));
    assert!(std::ptr::eq(pair.first_socket(), &first));
    assert!(std::ptr::eq(pair.second_socket(), &second));
    assert_eq!(pair.first_socket().0, "first-pair-endpoint");
    assert_eq!(pair.second_socket().0, "second-pair-endpoint");

    assert_eq!(SocketListenBacklog::try_from_clamped(-1), None);
    let backlog = SocketListenBacklog::try_from_clamped(128).unwrap();
    assert_eq!(backlog.get(), 128);
    let listening = ListeningSocket(String::from("listening-endpoint"));
    let listen = SocketListenContext::new(&actor, &listening, backlog);
    assert!(std::ptr::eq(listen.actor(), actor.as_ref()));
    assert!(std::ptr::eq(listen.socket(), &listening));
    assert_eq!(listen.socket().0, "listening-endpoint");
    assert_eq!(listen.backlog(), backlog);

    let accepted = AcceptedSocket(String::from("new-accept-endpoint"));
    let accept = SocketAcceptContext::new(&actor, &listening, &accepted);
    assert!(std::ptr::eq(accept.actor(), actor.as_ref()));
    assert!(std::ptr::eq(accept.listening_socket(), &listening));
    assert!(std::ptr::eq(accept.new_socket(), &accepted));
    assert_eq!(accept.listening_socket().0, "listening-endpoint");
    assert_eq!(accept.new_socket().0, "new-accept-endpoint");
}

#[test]
fn socket_operation_contracts_borrow_prepared_objects_and_preserve_raw_facts() {
    let namespace = TestNamespace::initial("socket-operations");
    let actor = Credential::try_root(namespace).unwrap();
    let socket = NonCopyObject {
        identity: String::from("exact-pinned-socket"),
    };
    let address = PreparedSocketAddress {
        bytes: vec![2, 0, 0x1f, 0x90, 127, 0, 0, 1],
    };

    let bind = SocketBindContext::new(&actor, &socket, &address, address.bytes.len());
    let (borrowed_actor, borrowed_socket, borrowed_address) = borrow_bind_parts(&bind);
    assert!(std::ptr::eq(borrowed_actor, actor.as_ref()));
    assert!(std::ptr::eq(borrowed_socket, &socket));
    assert!(std::ptr::eq(borrowed_address, &address));
    assert_eq!(bind.address_length(), 8);

    let connect = SocketConnectContext::new(&actor, &socket, &address, address.bytes.len());
    assert!(std::ptr::eq(connect.actor(), actor.as_ref()));
    assert!(std::ptr::eq(connect.socket(), &socket));
    assert!(std::ptr::eq(connect.address(), &address));
    assert_eq!(connect.address_length(), address.bytes.len());

    let send_message = PreparedSocketMessage {
        identity: String::from("prepared-send-message"),
        normalized_flags: 0x4000,
    };
    let send = SocketSendMessageContext::new(&actor, &socket, &send_message, 4096);
    assert!(std::ptr::eq(send.actor(), actor.as_ref()));
    assert!(std::ptr::eq(send.socket(), &socket));
    assert!(std::ptr::eq(send.prepared_message(), &send_message));
    assert_eq!(send.prepared_message().identity, "prepared-send-message");
    assert_eq!(send.prepared_message().normalized_flags, 0x4000);
    assert_eq!(send.size(), 4096);

    let receive_message = PreparedSocketMessage {
        identity: String::from("prepared-receive-message"),
        normalized_flags: 0,
    };
    let receive = SocketReceiveMessageContext::new(&actor, &socket, &receive_message, 8192, -7);
    assert!(std::ptr::eq(receive.actor(), actor.as_ref()));
    assert!(std::ptr::eq(receive.socket(), &socket));
    assert!(std::ptr::eq(receive.prepared_message(), &receive_message));
    assert_eq!(receive.size(), 8192);
    assert_eq!(receive.flags(), -7);

    let get_name = SocketGetSockNameContext::new(&actor, &socket);
    let get_peer_name = SocketGetPeerNameContext::new(&actor, &socket);
    assert!(std::ptr::eq(get_name.actor(), actor.as_ref()));
    assert!(std::ptr::eq(get_name.socket(), &socket));
    assert!(std::ptr::eq(get_peer_name.actor(), actor.as_ref()));
    assert!(std::ptr::eq(get_peer_name.socket(), &socket));

    let option = SocketOption::new(1, 7);
    assert_eq!(option.level(), 1);
    assert_eq!(option.name(), 7);
    let get_option = SocketGetOptionContext::new(&actor, &socket, option);
    let set_option = SocketSetOptionContext::new(&actor, &socket, option);
    assert!(std::ptr::eq(get_option.actor(), actor.as_ref()));
    assert!(std::ptr::eq(get_option.socket(), &socket));
    assert_eq!(get_option.option(), option);
    assert!(std::ptr::eq(set_option.actor(), actor.as_ref()));
    assert!(std::ptr::eq(set_option.socket(), &socket));
    assert_eq!(set_option.option(), option);

    let shutdown = SocketShutdownContext::new(&actor, &socket, -19);
    assert!(std::ptr::eq(shutdown.actor(), actor.as_ref()));
    assert!(std::ptr::eq(shutdown.socket(), &socket));
    assert_eq!(shutdown.how(), -19);
}

#[test]
fn unix_socket_contracts_keep_connect_and_send_directions_distinct() {
    let namespace = TestNamespace::initial("unix-socket-roles");
    let actor = Credential::try_root(namespace).unwrap();
    let connecting = ConnectingUnixSocket(String::from("connecting-client"));
    let listening = ListeningUnixSocket(String::from("listening-server"));
    let accepted = AcceptedUnixSocket(String::from("new-server-child"));
    let stream = UnixStreamConnectContext::new(&actor, &connecting, &listening, &accepted);
    let (borrowed_connecting, borrowed_listening, borrowed_accepted) =
        borrow_unix_stream_roles(&stream);
    assert!(std::ptr::eq(stream.actor(), actor.as_ref()));
    assert!(std::ptr::eq(borrowed_connecting, &connecting));
    assert!(std::ptr::eq(borrowed_listening, &listening));
    assert!(std::ptr::eq(borrowed_accepted, &accepted));
    assert_eq!(borrowed_connecting.0, "connecting-client");
    assert_eq!(borrowed_listening.0, "listening-server");
    assert_eq!(borrowed_accepted.0, "new-server-child");

    let sending = SendingUnixSocket(String::from("sending-endpoint"));
    let receiving = ReceivingUnixSocket(String::from("receiving-endpoint"));
    let may_send = UnixMaySendContext::new(&actor, &sending, &receiving);
    assert!(std::ptr::eq(may_send.actor(), actor.as_ref()));
    assert!(std::ptr::eq(may_send.sending_socket(), &sending));
    assert!(std::ptr::eq(may_send.receiving_socket(), &receiving));
    assert_eq!(may_send.sending_socket().0, "sending-endpoint");
    assert_eq!(may_send.receiving_socket().0, "receiving-endpoint");
}

#[test]
fn mmap_security_contracts_are_strict_lossless_and_exactly_borrowed() {
    assert_eq!(
        MemoryProtection::try_from_bits(0),
        Some(MemoryProtection::NONE)
    );
    let requested = MemoryProtection::READ | MemoryProtection::WRITE;
    let effective = requested | MemoryProtection::EXECUTE;
    assert_eq!(effective, MemoryProtection::ALL);
    assert_eq!(MemoryProtection::try_from_bits(0x7), Some(effective));
    assert_eq!(MemoryProtection::try_from_bits(0x8), None);
    assert_eq!(MemoryProtection::try_from_bits(usize::MAX), None);

    let flags = MmapFileFlags::from_raw(usize::MAX);
    assert_eq!(flags.raw(), usize::MAX);
    let operation = MmapFileOperation::new(requested, effective, flags);
    assert_eq!(operation.requested(), requested);
    assert_eq!(operation.effective(), effective);
    assert_eq!(operation.flags().raw(), usize::MAX);

    let actor_namespace = TestNamespace::initial("mmap-actor");
    let filesystem_namespace = TestNamespace::initial("mmap-filesystem-owner");
    let image_namespace = TestNamespace::initial("mmap-image-owner");
    let actor = Credential::try_root(actor_namespace.clone()).unwrap();
    let file = MappedFile(String::from("exact-mapped-file"));

    let anonymous: MmapFileContext<'_, TestNamespace, MappedFile> =
        MmapFileContext::new(&actor, MmapFileTarget::Anonymous, operation);
    assert!(anonymous.target().is_anonymous());
    assert!(anonymous.target().file_security_ref().is_none());
    assert!(anonymous.target().file_object().is_none());
    assert!(anonymous.target().filesystem_owner_user_ns().is_none());

    let file_target = MmapFileTarget::File(MmapFileSecurityRef::new(&filesystem_namespace, &file));
    let file_context = MmapFileContext::new(&actor, file_target, operation);
    let (borrowed_actor, borrowed_filesystem_namespace, borrowed_file) =
        borrow_mmap_file_parts(&file_context);
    assert!(std::ptr::eq(borrowed_actor, actor.as_ref()));
    assert!(Arc::ptr_eq(
        borrowed_filesystem_namespace,
        &filesystem_namespace
    ));
    assert!(!Arc::ptr_eq(
        borrowed_filesystem_namespace,
        &actor_namespace
    ));
    assert!(std::ptr::eq(borrowed_file, &file));
    assert_eq!(borrowed_file.0, "exact-mapped-file");
    assert_eq!(file_context.operation(), operation);

    let image = MemoryImage(String::from("exact-address-space-image"));
    let address = MmapAddressContext::new(&actor, &image_namespace, &image, 0x7fff_4000);
    assert!(std::ptr::eq(address.actor(), actor.as_ref()));
    assert!(Arc::ptr_eq(address.image_owner_user_ns(), &image_namespace));
    assert!(std::ptr::eq(address.image(), &image));
    assert_eq!(address.image().0, "exact-address-space-image");
    assert_eq!(address.final_address(), 0x7fff_4000);

    let pre_change_vma = PreChangeVma {
        identity: String::from("exact-pre-change-vma"),
        old_protection: MemoryProtection::READ,
    };
    let mprotect = FileMprotectContext::new(
        &actor,
        &image_namespace,
        &pre_change_vma,
        requested,
        effective,
    );
    assert!(std::ptr::eq(mprotect.actor(), actor.as_ref()));
    assert!(Arc::ptr_eq(
        mprotect.image_owner_user_ns(),
        &image_namespace
    ));
    assert!(std::ptr::eq(mprotect.pre_change_vma(), &pre_change_vma));
    assert_eq!(mprotect.pre_change_vma().identity, "exact-pre-change-vma");
    assert_eq!(
        mprotect.pre_change_vma().old_protection,
        MemoryProtection::READ
    );
    assert_eq!(mprotect.requested(), requested);
    assert_eq!(mprotect.effective(), effective);
}
