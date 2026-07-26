//! Host tests for the key manager.

use alloc::{string::String, vec};

use thekernel_linux_cred::{CAPABILITY_WORDS, GroupInfo};

use super::*;

fn actor(tid: u32, pid: u32, uid: u32, gid: u32) -> KeyActor {
    actor_in_namespace(tid, pid, uid, gid, UserNamespace::try_new_root().unwrap())
}

fn actor_in_namespace(
    tid: u32,
    pid: u32,
    uid: u32,
    gid: u32,
    user_ns: Arc<UserNamespace>,
) -> KeyActor {
    let uid = Kuid::from_raw(uid).unwrap();
    let gid = Kgid::from_raw(gid).unwrap();
    let groups = GroupInfo::try_new(Vec::new()).unwrap();
    KeyActor {
        tid,
        pid,
        thread_owner: tid,
        process_owner: pid,
        ids: Credentials {
            ruid: uid,
            euid: uid,
            suid: uid,
            fsuid: uid,
            rgid: gid,
            egid: gid,
            sgid: gid,
            fsgid: gid,
        },
        dac: DacCredentialView::new(uid, gid, groups, [0; CAPABILITY_WORDS], true),
        user_ns,
        has_sys_admin: false,
        has_setuid: false,
    }
}

fn keyctl_value(output: KeyctlOutput) -> isize {
    match output {
        KeyctlOutput::Value(value) => value,
        _ => panic!("expected scalar keyctl output"),
    }
}

fn keyctl_counted_bytes(output: KeyctlOutput) -> Vec<u8> {
    match output {
        KeyctlOutput::CountedBytes(bytes) => bytes,
        _ => panic!("expected counted keyctl output"),
    }
}

fn assert_accounting_consistent(manager: &KeyManager) {
    let mut owners = BTreeMap::<Kuid, OwnerUsage>::new();
    let mut budget = ManagerBudgetUsage::default();
    let mut root_refs = BTreeMap::<i32, usize>::new();
    let mut link_refs = BTreeMap::<i32, usize>::new();

    for (serial, key) in &manager.keys {
        if key.in_owner_quota {
            let usage = owners.entry(key.quota_uid).or_default();
            usage.keys += key.abi_charge.keys;
            usage.bytes += key.abi_charge.bytes;
        }
        budget.objects += key.resident_charge.objects;
        budget.bytes += key.resident_charge.bytes;
        budget.link_bytes += key.resident_charge.link_bytes;
        for linked in &key.links {
            *link_refs.entry(*linked).or_default() += 1;
        }
        root_refs.entry(*serial).or_default();
        link_refs.entry(*serial).or_default();
    }

    for serial in manager
        .thread_keyrings
        .values()
        .chain(manager.process_keyrings.values())
        .chain(manager.session_keyrings.values())
        .chain(
            manager
                .namespaces
                .values()
                .flat_map(NamespaceRegistry::root_serials),
        )
    {
        *root_refs.entry(*serial).or_default() += 1;
    }

    assert_eq!(manager.owners.usage.len(), owners.len());
    for (uid, usage) in owners {
        assert_eq!(manager.owners.usage(uid), usage);
    }
    assert!(manager.owners.gc_scratch_is_idle());
    assert_eq!(manager.budget.used, budget);
    for (serial, key) in &manager.keys {
        assert_eq!(key.root_refs, root_refs[serial]);
        assert_eq!(key.link_refs, link_refs[serial]);
        assert!(key.gc_plan.is_idle());
        assert_eq!(key.gc_next, None);
        assert_eq!(
            key.resident_charge.link_bytes,
            key.links.capacity() * KEY_LINK_CHARGE
        );
    }
}

/// Serial, root count, link count, links, GC scratch, and owner for one key.
type GcKeyState = (i32, usize, usize, Vec<i32>, GcPlanScratch, Option<i32>);

fn gc_key_state(manager: &KeyManager) -> Vec<GcKeyState> {
    manager
        .keys
        .iter()
        .map(|(serial, key)| {
            (
                *serial,
                key.root_refs,
                key.link_refs,
                key.links.clone(),
                key.gc_plan,
                key.gc_next,
            )
        })
        .collect()
}

fn assert_prepared_gc_scratch_idle(manager: &KeyManager) {
    assert!(manager.owners.gc_scratch_is_idle());
    assert!(manager.keys.values().all(|key| key.gc_plan.is_idle()));
}

#[test]
fn quota_sysctl_limits_match_linux_int_range() {
    assert_eq!(validate_key_quota_limit(1), Ok(1));
    assert_eq!(
        validate_key_quota_limit(i32::MAX as usize),
        Ok(i32::MAX as usize)
    );
    assert_eq!(validate_key_quota_limit(0), Err(AxError::InvalidInput));
    assert_eq!(
        validate_key_quota_limit(i32::MAX as usize + 1),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn only_the_first_anonymous_session_install_allows_quota_overrun() {
    let owner = actor(1, 1, 1000, 1000);
    let mut manager = KeyManager::new();
    assert_eq!(
        manager.anonymous_session_admission(owner.thread_owner),
        QuotaAdmission::AllowOverrun
    );
    let session = manager
        .try_create_rooted_keyring(
            RootSource::Session(owner.thread_owner),
            "_ses.1".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            anonymous_session_keyring_permissions(),
            QuotaAdmission::AllowOverrun,
        )
        .unwrap();
    assert_eq!(
        manager.anonymous_session_admission(owner.thread_owner),
        QuotaAdmission::Enforced
    );
    assert_eq!(manager.session_keyrings[&owner.thread_owner], session);
    assert_accounting_consistent(&manager);
}

#[test]
fn session_lookup_without_create_installs_the_user_session_keyring() {
    let owner = actor(2, 2, 1000, 1000);
    let mut manager = KeyManager::new();

    let session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &owner, false)
        .unwrap();

    assert_eq!(manager.session_keyrings[&owner.thread_owner], session);
    assert_eq!(
        manager.namespaces[&owner.user_ns.identity()].user_session_keyrings[&owner.real_uid()],
        session
    );
    assert_eq!(manager.keys[&session].root_refs, 2);
    assert_accounting_consistent(&manager);
}

#[test]
fn initial_special_keyrings_use_real_ids_after_fsids_diverge() {
    let mut owner = actor(3, 3, 1000, 1001);
    let fsuid = Kuid::from_raw(2000).unwrap();
    let fsgid = Kgid::from_raw(2001).unwrap();
    owner.ids.fsuid = fsuid;
    owner.ids.fsgid = fsgid;
    owner.dac = DacCredentialView::new(
        fsuid,
        fsgid,
        GroupInfo::try_new(Vec::new()).unwrap(),
        [0; CAPABILITY_WORDS],
        true,
    );

    let mut manager = KeyManager::new();
    let thread = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let process = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
        .unwrap();
    let session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &owner, true)
        .unwrap();
    for serial in [thread, process, session] {
        let key = &manager.keys[&serial];
        assert_eq!(key.uid, owner.ids.ruid);
        assert_eq!(key.quota_uid, owner.ids.ruid);
        assert_eq!(key.owner_gid, Some(owner.ids.rgid));
    }

    let named_session = keyctl_value(
        manager
            .keyctl(
                &owner,
                KeyctlCommand::JoinSession {
                    name: Some("real-owner".to_string()),
                },
            )
            .unwrap(),
    ) as i32;
    let named_session = &manager.keys[&named_session];
    assert_eq!(named_session.uid, owner.ids.ruid);
    assert_eq!(named_session.quota_uid, owner.ids.ruid);
    assert_eq!(named_session.owner_gid, Some(owner.ids.rgid));

    let ordinary = manager
        .add_key(
            &owner,
            KeyTypeKind::User,
            "fs-owner".to_string(),
            vec![1],
            KEY_SPEC_THREAD_KEYRING,
        )
        .unwrap() as i32;
    assert_eq!(manager.keys[&ordinary].uid, fsuid);
    assert_eq!(manager.keys[&ordinary].quota_uid, fsuid);
    assert_eq!(manager.keys[&ordinary].owner_gid, Some(fsgid));

    let user = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    let user_session = manager
        .special_keyring(KEY_SPEC_USER_SESSION_KEYRING, &owner, true)
        .unwrap();
    let persistent = manager
        .get_persistent_keyring(owner.real_uid(), &owner)
        .unwrap()
        .serial;
    for serial in [user, user_session, persistent] {
        assert_eq!(manager.keys[&serial].owner_gid, None);
    }
    assert_accounting_consistent(&manager);
}

#[test]
fn absent_owner_gid_never_selects_group_permission_lane() {
    let member = actor(4, 4, 1001, 2000);
    let mut manager = KeyManager::new();
    let serial = manager.insert_key(Key::keyring_without_group(
        "no-group-owner".to_string(),
        Kuid::from_raw(1000).unwrap(),
        KeyPermissionMask::from_lanes(
            None,
            None,
            Some(KeyPermission::READ),
            Some(KeyPermission::VIEW),
        ),
    ));

    assert_eq!(manager.keys[&serial].owner_gid, None);
    assert!(
        !manager
            .key_has_perm(serial, &member, KeyPermission::READ)
            .unwrap()
    );
    assert!(
        manager
            .key_has_perm(serial, &member, KeyPermission::VIEW)
            .unwrap()
    );
    assert_accounting_consistent(&manager);
}

#[test]
fn describe_uses_overflow_gid_for_absent_and_unmapped_group_owners() {
    let root_actor = actor(5, 5, 1000, 1000);
    let mut manager = KeyManager::new();
    let no_group = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &root_actor, true)
        .unwrap();
    assert_eq!(manager.keys[&no_group].owner_gid, None);
    let description = String::from_utf8(keyctl_counted_bytes(
        manager
            .keyctl(&root_actor, KeyctlCommand::Describe { key: no_group })
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(description.split(';').nth(2), Some("65534"));

    let child_namespace = root_actor
        .user_ns
        .try_fork(
            Kuid::from_raw(1000).unwrap(),
            Kgid::from_raw(1000).unwrap(),
            false,
        )
        .unwrap();
    let uid_map = child_namespace
        .try_build_uid_map(vec![crate::task::IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    child_namespace.publish_uid_map(uid_map).unwrap();
    let child_actor = actor_in_namespace(6, 6, 1000, 1000, child_namespace);
    let unmapped_group = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &child_actor, true)
        .unwrap();
    assert_eq!(
        manager.keys[&unmapped_group].owner_gid,
        Some(child_actor.real_gid())
    );
    let description = String::from_utf8(keyctl_counted_bytes(
        manager
            .keyctl(
                &child_actor,
                KeyctlCommand::Describe {
                    key: unmapped_group,
                },
            )
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(description.split(';').nth(2), Some("65534"));
    assert_accounting_consistent(&manager);
}

#[test]
fn chown_no_change_precedes_namespace_lookup_and_valid_gid_becomes_present() {
    let owner = actor(7, 7, 1000, 1001);
    let mut manager = KeyManager::new();
    assert!(manager.namespaces.is_empty());
    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &owner,
                    KeyctlCommand::Chown {
                        key: i32::MAX,
                        uid: None,
                        gid: None,
                    },
                )
                .unwrap()
        ),
        0
    );
    assert!(manager.namespaces.is_empty());

    let child_namespace = owner
        .user_ns
        .try_fork(owner.real_uid(), owner.real_gid(), false)
        .unwrap();
    let uid_map = child_namespace
        .try_build_uid_map(vec![crate::task::IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    child_namespace.publish_uid_map(uid_map).unwrap();
    let child_actor = actor_in_namespace(8, 8, 1000, 1001, child_namespace);
    let error = match manager.keyctl(
        &child_actor,
        KeyctlCommand::Chown {
            key: i32::MAX,
            uid: Some(1),
            gid: None,
        },
    ) {
        Err(error) => error,
        Ok(_) => panic!("unmapped chown owner unexpectedly succeeded"),
    };
    assert_eq!(error, AxError::InvalidInput);
    assert!(manager.namespaces.is_empty());

    let user = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    assert_eq!(manager.keys[&user].owner_gid, None);
    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &owner,
                    KeyctlCommand::Chown {
                        key: user,
                        uid: None,
                        gid: Some(owner.real_gid().into_raw()),
                    },
                )
                .unwrap()
        ),
        0
    );
    assert_eq!(manager.keys[&user].owner_gid, Some(owner.real_gid()));
    assert_accounting_consistent(&manager);
}

#[test]
fn session_and_reqkey_subscriptions_are_isolated_by_immutable_task_owner() {
    let namespace = UserNamespace::try_new_root().unwrap();
    let first = actor_in_namespace(10, 77, 1000, 1000, namespace.clone());
    let second = actor_in_namespace(11, 77, 1000, 1000, namespace);
    let mut manager = KeyManager::new();

    let first_session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &first, true)
        .unwrap();
    let second_session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &second, true)
        .unwrap();
    assert_ne!(first_session, second_session);
    assert_eq!(manager.session_keyrings[&first.thread_owner], first_session);
    assert_eq!(
        manager.session_keyrings[&second.thread_owner],
        second_session
    );

    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &first,
                    KeyctlCommand::SetReqKeyring {
                        setting: ReqKeyDefault::Session,
                    },
                )
                .unwrap(),
        ),
        ReqKeyDefault::Default as isize
    );
    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &second,
                    KeyctlCommand::SetReqKeyring {
                        setting: ReqKeyDefault::NoChange,
                    },
                )
                .unwrap(),
        ),
        ReqKeyDefault::Default as isize
    );

    let mut namespace_shifted = first.clone();
    namespace_shifted.user_ns = UserNamespace::try_new_root().unwrap();
    assert_eq!(
        manager.special_keyring(KEY_SPEC_SESSION_KEYRING, &namespace_shifted, false),
        Ok(first_session)
    );
    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &namespace_shifted,
                    KeyctlCommand::SetReqKeyring {
                        setting: ReqKeyDefault::NoChange,
                    },
                )
                .unwrap(),
        ),
        ReqKeyDefault::Session as isize
    );
    assert_accounting_consistent(&manager);
}

#[test]
fn ordinary_fork_inherits_only_session_and_reqkey_and_rolls_back_exactly() {
    let parent = actor(40, 40, 1000, 1001);
    let parent_owner = KeyTaskOwner::new(parent.thread_owner, parent.process_owner);
    let child_owner = KeyTaskOwner::new(41, 41);
    let mut manager = KeyManager::new();
    let parent_thread = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &parent, true)
        .unwrap();
    let parent_process = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &parent, true)
        .unwrap();
    let session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &parent, true)
        .unwrap();
    manager
        .reqkey_defaults
        .insert(parent.thread_owner, ReqKeyDefault::Session as i32);
    let session_refs = manager.keys[&session].root_refs;

    let undo = manager
        .prepare_fork(
            parent_owner,
            child_owner,
            false,
            parent.real_uid(),
            parent.real_gid(),
        )
        .unwrap();
    assert!(!manager.thread_keyrings.contains_key(&41));
    assert!(!manager.process_keyrings.contains_key(&41));
    assert_eq!(manager.session_keyrings[&41], session);
    assert_eq!(manager.reqkey_defaults[&41], ReqKeyDefault::Session as i32);
    assert_eq!(manager.keys[&session].root_refs, session_refs + 1);
    assert_eq!(manager.thread_keyrings[&40], parent_thread);
    assert_eq!(manager.process_keyrings[&40], parent_process);

    manager.rollback_fork(undo).unwrap();
    assert!(!manager.thread_keyrings.contains_key(&41));
    assert!(!manager.process_keyrings.contains_key(&41));
    assert!(!manager.session_keyrings.contains_key(&41));
    assert!(!manager.reqkey_defaults.contains_key(&41));
    assert_eq!(manager.keys[&session].root_refs, session_refs);
    assert_accounting_consistent(&manager);
}

#[test]
fn clone_thread_creates_a_fresh_thread_ring_only_when_parent_has_one() {
    let namespace = UserNamespace::try_new_root().unwrap();
    let parent = actor_in_namespace(50, 77, 1000, 1001, namespace.clone());
    let child = actor_in_namespace(51, 77, 1000, 1001, namespace);
    let parent_owner = KeyTaskOwner::new(parent.thread_owner, parent.process_owner);
    let child_owner = KeyTaskOwner::new(child.thread_owner, child.process_owner);
    let mut manager = KeyManager::new();
    let process = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &parent, true)
        .unwrap();
    let session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &parent, true)
        .unwrap();
    manager
        .reqkey_defaults
        .insert(parent.thread_owner, ReqKeyDefault::Process as i32);

    let no_thread = manager
        .prepare_fork(
            parent_owner,
            child_owner,
            true,
            child.real_uid(),
            child.real_gid(),
        )
        .unwrap();
    assert!(!manager.thread_keyrings.contains_key(&child.thread_owner));
    assert_eq!(manager.session_keyrings[&child.thread_owner], session);
    assert_eq!(
        manager.reqkey_defaults[&child.thread_owner],
        ReqKeyDefault::Process as i32
    );
    assert_eq!(manager.process_keyrings[&parent.process_owner], process);
    manager.rollback_fork(no_thread).unwrap();

    let parent_thread = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &parent, true)
        .unwrap();
    let _committed = manager
        .prepare_fork(
            parent_owner,
            child_owner,
            true,
            child.real_uid(),
            child.real_gid(),
        )
        .unwrap();
    let child_thread = manager.thread_keyrings[&child.thread_owner];
    assert_ne!(child_thread, parent_thread);
    let child_key = &manager.keys[&child_thread];
    assert!(child_key.links.is_empty());
    assert_eq!(child_key.uid, child.real_uid());
    assert_eq!(child_key.quota_uid, child.real_uid());
    assert_eq!(child_key.owner_gid, Some(child.real_gid()));
    assert_eq!(manager.session_keyrings[&child.thread_owner], session);
    assert_eq!(manager.process_keyrings[&parent.process_owner], process);

    // Manager preparation has already staged the committed state; the
    // service token owns whether this undo record is consumed or applied.
    assert_accounting_consistent(&manager);
}

#[test]
fn exec_and_exit_retire_only_their_linux_lifecycle_roots() {
    let namespace = UserNamespace::try_new_root().unwrap();
    let leader = actor_in_namespace(60, 60, 1000, 1000, namespace.clone());
    let sibling = actor_in_namespace(61, 60, 1000, 1000, namespace);
    let mut manager = KeyManager::new();
    let leader_thread = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &leader, true)
        .unwrap();
    let process = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &leader, true)
        .unwrap();
    let session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &leader, true)
        .unwrap();
    manager
        .reqkey_defaults
        .insert(leader.thread_owner, ReqKeyDefault::Session as i32);
    let _sibling_undo = manager
        .prepare_fork(
            KeyTaskOwner::new(leader.thread_owner, leader.process_owner),
            KeyTaskOwner::new(sibling.thread_owner, sibling.process_owner),
            true,
            sibling.real_uid(),
            sibling.real_gid(),
        )
        .unwrap();
    let sibling_thread = manager.thread_keyrings[&sibling.thread_owner];

    manager
        .exit_committed(
            KeyTaskOwner::new(sibling.thread_owner, sibling.process_owner),
            false,
        )
        .unwrap();
    assert!(!manager.keys.contains_key(&sibling_thread));
    assert_eq!(manager.process_keyrings[&leader.process_owner], process);
    assert_eq!(manager.session_keyrings[&leader.thread_owner], session);
    assert!(!manager.reqkey_defaults.contains_key(&sibling.thread_owner));

    manager
        .exec_committed(KeyTaskOwner::new(leader.thread_owner, leader.process_owner))
        .unwrap();
    assert!(!manager.keys.contains_key(&leader_thread));
    assert!(!manager.process_keyrings.contains_key(&leader.process_owner));
    assert_eq!(manager.session_keyrings[&leader.thread_owner], session);
    assert_eq!(
        manager.reqkey_defaults[&leader.thread_owner],
        ReqKeyDefault::Session as i32
    );
    let post_exec_process = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &leader, true)
        .unwrap();

    manager
        .exit_committed(
            KeyTaskOwner::new(leader.thread_owner, leader.process_owner),
            true,
        )
        .unwrap();
    assert!(!manager.session_keyrings.contains_key(&leader.thread_owner));
    assert!(!manager.process_keyrings.contains_key(&leader.process_owner));
    assert!(!manager.reqkey_defaults.contains_key(&leader.thread_owner));
    assert!(!manager.keys.contains_key(&session));
    assert!(!manager.keys.contains_key(&post_exec_process));
    assert_accounting_consistent(&manager);
}

#[test]
fn task_root_gc_missing_grandchild_is_zero_mutation() {
    let owner = actor(62, 62, 1000, 1000);
    let task_owner = KeyTaskOwner::new(owner.thread_owner, owner.process_owner);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let branch = manager.create_keyring(
        "branch".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let grandchild = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "grandchild".to_string(),
        vec![0x5a],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, branch).unwrap();
    manager.link_key_replace(branch, grandchild).unwrap();
    drop(manager.keys.remove(&grandchild).unwrap());

    let thread_roots = manager.thread_keyrings.clone();
    let process_roots = manager.process_keyrings.clone();
    let session_roots = manager.session_keyrings.clone();
    let keys = gc_key_state(&manager);
    let owners = manager.owners.usage.clone();
    let budget = manager.budget.used;

    assert_eq!(manager.exec_committed(task_owner), Err(AxError::BadState));
    assert_eq!(manager.thread_keyrings, thread_roots);
    assert_eq!(manager.process_keyrings, process_roots);
    assert_eq!(manager.session_keyrings, session_roots);
    assert_eq!(gc_key_state(&manager), keys);
    assert_eq!(manager.owners.usage, owners);
    assert_eq!(manager.budget.used, budget);
    assert_prepared_gc_scratch_idle(&manager);
}

#[test]
fn prepared_task_gc_retires_a_diamond_once() {
    let owner = actor(63, 63, 1000, 1000);
    let task_owner = KeyTaskOwner::new(owner.thread_owner, owner.process_owner);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let left = manager.create_keyring(
        "left".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let right = manager.create_keyring(
        "right".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let shared = manager.create_keyring(
        "shared".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    manager.link_key_replace(root, left).unwrap();
    manager.link_key_replace(root, right).unwrap();
    manager.link_key_replace(left, shared).unwrap();
    manager.link_key_replace(right, shared).unwrap();
    assert_eq!(manager.keys[&shared].link_refs, 2);

    manager.exec_committed(task_owner).unwrap();

    for serial in [root, left, right, shared] {
        assert!(!manager.keys.contains_key(&serial));
    }
    assert_accounting_consistent(&manager);
}

#[test]
fn prepared_task_gc_preserves_a_shared_child_with_a_live_root() {
    let owner = actor(64, 64, 1000, 1000);
    let task_owner = KeyTaskOwner::new(owner.thread_owner, owner.process_owner);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let shared = manager.create_keyring(
        "shared-survivor".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    manager
        .install_root(RootSource::Session(owner.thread_owner), shared)
        .unwrap();
    manager.link_key_replace(root, shared).unwrap();
    assert_eq!(manager.keys[&shared].root_refs, 1);
    assert_eq!(manager.keys[&shared].link_refs, 1);

    manager.exec_committed(task_owner).unwrap();

    assert!(!manager.keys.contains_key(&root));
    assert_eq!(manager.session_keyrings[&owner.thread_owner], shared);
    assert_eq!(manager.keys[&shared].root_refs, 1);
    assert_eq!(manager.keys[&shared].link_refs, 0);
    assert!(manager.keys[&shared].gc_plan.is_idle());
    assert_accounting_consistent(&manager);
}

#[test]
fn fsid_precommit_changes_only_the_changed_thread_owner_axis() {
    let mut owner = actor(70, 70, 1000, 1001);
    owner.has_sys_admin = true;
    let mut manager = KeyManager::new();
    let thread = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let process = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
        .unwrap();
    let session = manager
        .special_keyring(KEY_SPEC_SESSION_KEYRING, &owner, true)
        .unwrap();
    let quota_uid = manager.keys[&thread].quota_uid;
    let quota_before = manager.owners.usage(quota_uid);
    let process_owner = (manager.keys[&process].uid, manager.keys[&process].owner_gid);
    let session_owner = (manager.keys[&session].uid, manager.keys[&session].owner_gid);
    let fsuid = Kuid::from_raw(2000).unwrap();
    let fsgid = Kgid::from_raw(2001).unwrap();
    let chown_uid = Kuid::from_raw(3000).unwrap();
    let chown_gid = Kgid::from_raw(3001).unwrap();

    keyctl_value(
        manager
            .keyctl(
                &owner,
                KeyctlCommand::Chown {
                    key: thread,
                    uid: None,
                    gid: Some(chown_gid.into_raw()),
                },
            )
            .unwrap(),
    );
    manager
        .credential_fsids_precommit(owner.thread_owner, Some(fsuid), None)
        .unwrap();
    assert_eq!(manager.keys[&thread].uid, fsuid);
    assert_eq!(manager.keys[&thread].owner_gid, Some(chown_gid));
    assert_eq!(manager.keys[&thread].quota_uid, quota_uid);
    assert_eq!(manager.owners.usage(quota_uid), quota_before);

    keyctl_value(
        manager
            .keyctl(
                &owner,
                KeyctlCommand::Chown {
                    key: thread,
                    uid: Some(chown_uid.into_raw()),
                    gid: None,
                },
            )
            .unwrap(),
    );
    manager
        .credential_fsids_precommit(owner.thread_owner, None, Some(fsgid))
        .unwrap();
    assert_eq!(manager.keys[&thread].uid, chown_uid);
    assert_eq!(manager.keys[&thread].owner_gid, Some(fsgid));
    assert_eq!(manager.keys[&thread].quota_uid, chown_uid);
    assert_eq!(
        (manager.keys[&process].uid, manager.keys[&process].owner_gid),
        process_owner
    );
    assert_eq!(
        (manager.keys[&session].uid, manager.keys[&session].owner_gid),
        session_owner
    );
    assert_accounting_consistent(&manager);
}

#[test]
fn namespace_roots_are_isolated_and_dead_registry_pruning_reclaims_them() {
    let owner = actor(20, 20, 1000, 1000);
    let namespace = owner.user_ns.identity();
    let mut manager = KeyManager::new();
    let user = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    let user_session = manager
        .special_keyring(KEY_SPEC_USER_SESSION_KEYRING, &owner, true)
        .unwrap();
    let persistent = manager
        .get_persistent_keyring(owner.real_uid(), &owner)
        .unwrap()
        .serial;
    assert!(manager.namespaces.contains_key(&namespace));

    drop(owner);
    assert!(manager.key_user_records().unwrap().is_empty());
    assert!(!manager.namespaces.contains_key(&namespace));
    for serial in [user, user_session, persistent] {
        assert!(!manager.keys.contains_key(&serial));
    }

    let replacement = actor(21, 21, 1000, 1000);
    let replacement_user = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &replacement, true)
        .unwrap();
    assert_ne!(replacement_user, user);
    assert_ne!(replacement.user_ns.identity(), namespace);
    assert_accounting_consistent(&manager);
}

#[test]
fn namespace_root_descriptions_use_visible_not_global_uid() {
    let root = UserNamespace::try_new_root().unwrap();
    let child = root
        .try_fork(
            Kuid::from_raw(1000).unwrap(),
            Kgid::from_raw(1000).unwrap(),
            false,
        )
        .unwrap();
    let uid_map = child
        .try_build_uid_map(vec![crate::task::IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    child.publish_uid_map(uid_map).unwrap();
    let owner = actor_in_namespace(23, 23, 1000, 1000, child);
    let mut manager = KeyManager::new();

    let user = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    let user_session = manager
        .special_keyring(KEY_SPEC_USER_SESSION_KEYRING, &owner, true)
        .unwrap();
    let persistent = manager
        .get_persistent_keyring(owner.real_uid(), &owner)
        .unwrap()
        .serial;
    assert_eq!(manager.keys[&user].description, "_uid.0");
    assert_eq!(manager.keys[&user_session].description, "_uid_ses.0");
    assert_eq!(manager.keys[&persistent].description, "_persistent.0");
    assert_accounting_consistent(&manager);
}

#[test]
fn dead_namespace_prune_prevalidation_never_partially_detaches_roots() {
    let owner = actor(22, 22, 1000, 1000);
    let namespace = owner.user_ns.identity();
    let mut manager = KeyManager::new();
    let user = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    manager.keys.get_mut(&user).unwrap().root_refs = 0;
    drop(owner);

    assert_eq!(manager.key_user_records(), Err(AxError::BadState));
    assert_eq!(
        manager.namespaces[&namespace]
            .user_keyrings
            .values()
            .copied()
            .collect::<Vec<_>>(),
        vec![user]
    );
    assert!(manager.keys.contains_key(&user));
    assert!(manager.keys[&user].gc_plan.is_idle());
}

#[test]
fn namespace_prune_leaves_foreign_gc_scratch_untouched() {
    let owner = actor(24, 24, 1000, 1000);
    let namespace = owner.user_ns.identity();
    let mut manager = KeyManager::new();
    manager.ensure_namespace_registry(&owner).unwrap();
    let normal = manager
        .try_create_keyring(
            "normal-root".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            uid_keyring_permissions(),
            QuotaAdmission::Enforced,
        )
        .unwrap();
    let foreign = manager
        .try_create_keyring(
            "foreign-root".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            uid_keyring_permissions(),
            QuotaAdmission::Enforced,
        )
        .unwrap();
    manager
        .install_root(RootSource::User(namespace, owner.real_uid()), normal)
        .unwrap();
    manager
        .install_root(
            RootSource::UserSession(namespace, owner.real_uid()),
            foreign,
        )
        .unwrap();
    let foreign_scratch = GcPlanScratch {
        epoch: u64::MAX,
        root_drops: 7,
        link_drops: 3,
        state: Some(GcPlanState::Touched),
        touched_next: Some(1234),
        work_next: None,
    };
    manager.keys.get_mut(&foreign).unwrap().gc_plan = foreign_scratch;
    drop(owner);

    assert_eq!(manager.key_user_records(), Err(AxError::BadState));
    assert!(manager.namespaces.contains_key(&namespace));
    assert_eq!(manager.keys[&normal].root_refs, 1);
    assert!(manager.keys[&normal].gc_plan.is_idle());
    assert_eq!(manager.keys[&foreign].root_refs, 1);
    assert_eq!(manager.keys[&foreign].gc_plan, foreign_scratch);
}

#[test]
fn namespace_prune_counts_duplicate_roots_and_retires_once() {
    let owner = actor(24, 24, 1000, 1000);
    let namespace = owner.user_ns.identity();
    let uid = owner.real_uid();
    let mut manager = KeyManager::new();
    manager.ensure_namespace_registry(&owner).unwrap();
    let serial = manager
        .try_create_keyring(
            "shared-root".to_string(),
            owner.owner_uid(),
            owner.owner_gid(),
            uid_keyring_permissions(),
            QuotaAdmission::Enforced,
        )
        .unwrap();
    manager
        .install_root(RootSource::User(namespace, uid), serial)
        .unwrap();
    manager
        .install_root(RootSource::UserSession(namespace, uid), serial)
        .unwrap();
    assert_eq!(manager.keys[&serial].root_refs, 2);
    drop(owner);

    assert!(manager.key_user_records().unwrap().is_empty());
    assert!(!manager.namespaces.contains_key(&namespace));
    assert!(!manager.keys.contains_key(&serial));
    assert_accounting_consistent(&manager);
}

#[test]
fn namespace_gc_link_ref_underflow_is_zero_mutation() {
    let owner = actor(25, 25, 1000, 1000);
    let namespace = owner.user_ns.identity();
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    let child = manager.create_keyring(
        "underflow-child".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        uid_keyring_permissions(),
    );
    manager.link_key_replace(root, child).unwrap();
    manager.keys.get_mut(&child).unwrap().link_refs = 0;

    let namespace_roots = manager.namespaces[&namespace]
        .root_serials()
        .copied()
        .collect::<Vec<_>>();
    let keys = gc_key_state(&manager);
    let owners = manager.owners.usage.clone();
    let budget = manager.budget.used;
    drop(owner);

    assert_eq!(manager.key_user_records(), Err(AxError::BadState));
    assert_eq!(
        manager.namespaces[&namespace]
            .root_serials()
            .copied()
            .collect::<Vec<_>>(),
        namespace_roots
    );
    assert_eq!(gc_key_state(&manager), keys);
    assert_eq!(manager.owners.usage, owners);
    assert_eq!(manager.budget.used, budget);
    assert_prepared_gc_scratch_idle(&manager);
}

#[test]
fn namespace_gc_owner_underflow_is_zero_mutation() {
    let owner = actor(26, 26, 1000, 1000);
    let namespace = owner.user_ns.identity();
    let uid = owner.owner_uid();
    let mut manager = KeyManager::new();
    manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    manager
        .owners
        .set_usage_for_test(uid, OwnerUsage::default());
    let namespace_roots = manager.namespaces[&namespace]
        .root_serials()
        .copied()
        .collect::<Vec<_>>();
    let keys = gc_key_state(&manager);
    let owners = manager.owners.usage.clone();
    let budget = manager.budget.used;
    drop(owner);

    assert_eq!(manager.key_user_records(), Err(AxError::BadState));
    assert_eq!(
        manager.namespaces[&namespace]
            .root_serials()
            .copied()
            .collect::<Vec<_>>(),
        namespace_roots
    );
    assert_eq!(gc_key_state(&manager), keys);
    assert_eq!(manager.owners.usage, owners);
    assert_eq!(manager.budget.used, budget);
    assert_prepared_gc_scratch_idle(&manager);
}

#[test]
fn namespace_gc_budget_underflow_is_zero_mutation() {
    let owner = actor(27, 27, 1000, 1000);
    let namespace = owner.user_ns.identity();
    let mut manager = KeyManager::new();
    manager
        .special_keyring(KEY_SPEC_USER_KEYRING, &owner, true)
        .unwrap();
    manager.budget.used = ManagerBudgetUsage::default();
    let namespace_roots = manager.namespaces[&namespace]
        .root_serials()
        .copied()
        .collect::<Vec<_>>();
    let keys = gc_key_state(&manager);
    let owners = manager.owners.usage.clone();
    let budget = manager.budget.used;
    drop(owner);

    assert_eq!(manager.key_user_records(), Err(AxError::BadState));
    assert_eq!(
        manager.namespaces[&namespace]
            .root_serials()
            .copied()
            .collect::<Vec<_>>(),
        namespace_roots
    );
    assert_eq!(gc_key_state(&manager), keys);
    assert_eq!(manager.owners.usage, owners);
    assert_eq!(manager.budget.used, budget);
    assert_prepared_gc_scratch_idle(&manager);
}

#[test]
fn namespace_prune_visits_each_dead_registry_once() {
    const DEAD_NAMESPACES: usize = 128;

    let mut manager = KeyManager::new();
    let mut namespaces = Vec::new();
    for _ in 0..DEAD_NAMESPACES {
        let namespace = UserNamespace::try_new_root().unwrap();
        assert!(
            manager
                .namespaces
                .insert(namespace.identity(), NamespaceRegistry::new(&namespace),)
                .is_none()
        );
        namespaces.push(namespace);
    }
    drop(namespaces);

    manager.namespace_prune_candidates = 0;
    let live = actor(25, 25, 1000, 1000);
    manager.ensure_namespace_registry(&live).unwrap();
    assert_eq!(manager.namespace_prune_candidates, DEAD_NAMESPACES);
    assert_eq!(manager.namespaces.len(), 1);
    assert!(manager.namespaces.contains_key(&live.user_ns.identity()));
}

#[test]
fn public_operations_validate_and_prune_the_actor_namespace_once() {
    let owner = actor(26, 26, 1000, 1000);
    let mut manager = KeyManager::new();

    manager.namespace_ensure_calls = 0;
    let serial = manager
        .add_key(
            &owner,
            KeyTypeKind::User,
            "single-ensure".to_string(),
            vec![1],
            KEY_SPEC_THREAD_KEYRING,
        )
        .unwrap() as i32;
    assert_eq!(manager.namespace_ensure_calls, 1);

    manager.namespace_ensure_calls = 0;
    assert_eq!(
        manager
            .request_key(
                &owner,
                KeyTypeKind::User,
                "single-ensure",
                false,
                KEY_SPEC_PROCESS_KEYRING,
            )
            .unwrap(),
        serial as isize
    );
    assert_eq!(manager.namespace_ensure_calls, 1);

    manager.namespace_ensure_calls = 0;
    manager
        .keyctl(
            &owner,
            KeyctlCommand::GetPersistent {
                uid: None,
                destination: KEY_SPEC_THREAD_KEYRING,
            },
        )
        .unwrap();
    assert_eq!(manager.namespace_ensure_calls, 1);
    assert_accounting_consistent(&manager);
}

#[test]
fn named_sessions_are_namespace_scoped_and_name_metadata_owns_no_root() {
    let first = actor(30, 30, 1000, 1000);
    let second = actor(31, 31, 1000, 1000);
    let mut manager = KeyManager::new();

    let first_serial = keyctl_value(
        manager
            .keyctl(
                &first,
                KeyctlCommand::JoinSession {
                    name: Some("shared-name".to_string()),
                },
            )
            .unwrap(),
    ) as i32;
    let second_serial = keyctl_value(
        manager
            .keyctl(
                &second,
                KeyctlCommand::JoinSession {
                    name: Some("shared-name".to_string()),
                },
            )
            .unwrap(),
    ) as i32;
    assert_ne!(first_serial, second_serial);
    assert_eq!(manager.keys[&first_serial].root_refs, 1);
    assert_eq!(manager.keys[&second_serial].root_refs, 1);
    assert_eq!(
        manager.keys[&first_serial]
            .published_name
            .unwrap()
            .namespace,
        first.user_ns.identity()
    );
    assert_eq!(
        manager.keys[&second_serial]
            .published_name
            .unwrap()
            .namespace,
        second.user_ns.identity()
    );

    manager.keys.get_mut(&first_serial).unwrap().perm = permission_mask(
        KeyPermission::ALL,
        KeyPermission::VIEW | KeyPermission::SEARCH,
    );
    let rejoined = keyctl_value(
        manager
            .keyctl(
                &first,
                KeyctlCommand::JoinSession {
                    name: Some("shared-name".to_string()),
                },
            )
            .unwrap(),
    ) as i32;
    assert_eq!(rejoined, first_serial);
    assert_eq!(manager.keys[&first_serial].root_refs, 1);

    manager
        .keyctl(&first, KeyctlCommand::JoinSession { name: None })
        .unwrap();
    assert!(!manager.keys.contains_key(&first_serial));
    assert!(manager.keys.contains_key(&second_serial));
    assert_accounting_consistent(&manager);
}

#[test]
fn empty_join_session_names_are_never_published_or_reused() {
    let owner = actor(33, 33, 1000, 1000);
    let mut manager = KeyManager::new();
    let first = keyctl_value(
        manager
            .keyctl(
                &owner,
                KeyctlCommand::JoinSession {
                    name: Some(String::new()),
                },
            )
            .unwrap(),
    ) as i32;
    assert_eq!(manager.keys[&first].published_name, None);

    let second = keyctl_value(
        manager
            .keyctl(
                &owner,
                KeyctlCommand::JoinSession {
                    name: Some(String::new()),
                },
            )
            .unwrap(),
    ) as i32;
    assert_ne!(first, second);
    assert!(!manager.keys.contains_key(&first));
    assert_eq!(manager.keys[&second].published_name, None);
    assert_accounting_consistent(&manager);
}

#[test]
fn named_join_does_not_reuse_a_possessor_only_keyring() {
    let owner = actor(34, 34, 1000, 1000);
    let mut manager = KeyManager::new();
    let possessor_only = manager
        .add_key(
            &owner,
            KeyTypeKind::Keyring,
            "direct-search-required".to_string(),
            Vec::new(),
            KEY_SPEC_THREAD_KEYRING,
        )
        .unwrap() as i32;
    assert!(manager.is_possessed(possessor_only, &owner));
    assert!(!manager.keys[&possessor_only].perm.allows(
        manager.keys[&possessor_only].uid,
        manager.keys[&possessor_only].owner_gid,
        &owner.dac,
        false,
        KeyPermission::SEARCH,
    ));

    let joined = keyctl_value(
        manager
            .keyctl(
                &owner,
                KeyctlCommand::JoinSession {
                    name: Some("direct-search-required".to_string()),
                },
            )
            .unwrap(),
    ) as i32;
    assert_ne!(joined, possessor_only);
    assert!(manager.keys.contains_key(&possessor_only));
    assert_accounting_consistent(&manager);
}

#[test]
fn named_join_skips_quota_owners_unmapped_in_the_publication_namespace() {
    let root = UserNamespace::try_new_root().unwrap();
    let first_namespace = root
        .try_fork(
            Kuid::from_raw(1000).unwrap(),
            Kgid::from_raw(1000).unwrap(),
            false,
        )
        .unwrap();
    let first_map = first_namespace
        .try_build_uid_map(vec![crate::task::IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    first_namespace.publish_uid_map(first_map).unwrap();
    let second_namespace = root
        .try_fork(
            Kuid::from_raw(2000).unwrap(),
            Kgid::from_raw(2000).unwrap(),
            false,
        )
        .unwrap();
    let second_map = second_namespace
        .try_build_uid_map(vec![crate::task::IdMapInputExtent::new(0, 2000, 1)])
        .unwrap();
    second_namespace.publish_uid_map(second_map).unwrap();

    let first = actor_in_namespace(35, 35, 1000, 1000, first_namespace);
    let mut second = actor_in_namespace(36, 36, 2000, 2000, second_namespace);
    second.has_sys_admin = true;
    let mut manager = KeyManager::new();
    let original = manager
        .add_key(
            &first,
            KeyTypeKind::Keyring,
            "mapped-owner".to_string(),
            Vec::new(),
            KEY_SPEC_THREAD_KEYRING,
        )
        .unwrap() as i32;
    manager.keys.get_mut(&original).unwrap().perm = KeyPermissionMask::from_lanes(
        Some(KeyPermission::ALL),
        Some(KeyPermission::ALL),
        Some(KeyPermission::ALL),
        Some(KeyPermission::ALL),
    );
    manager
        .keyctl(
            &second,
            KeyctlCommand::Chown {
                key: original,
                uid: Some(0),
                gid: None,
            },
        )
        .unwrap();
    let transferred_uid = Kuid::from_raw(2000).unwrap();
    assert_eq!(manager.keys[&original].quota_uid, transferred_uid);
    assert!(first.user_ns.kernel_uid_to_user(transferred_uid).is_none());

    let joined = keyctl_value(
        manager
            .keyctl(
                &first,
                KeyctlCommand::JoinSession {
                    name: Some("mapped-owner".to_string()),
                },
            )
            .unwrap(),
    ) as i32;
    assert_ne!(joined, original);
    assert!(manager.keys.contains_key(&original));
    assert_accounting_consistent(&manager);
}

#[test]
fn public_keyring_names_allow_duplicates_choose_oldest_and_rollback_failures() {
    let owner = actor(32, 32, 1000, 1000);
    let mut manager = KeyManager::new();
    manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
        .unwrap();

    let empty = manager
        .add_key(
            &owner,
            KeyTypeKind::Keyring,
            String::new(),
            Vec::new(),
            KEY_SPEC_THREAD_KEYRING,
        )
        .unwrap() as i32;
    assert_eq!(manager.keys[&empty].published_name, None);

    let first = manager
        .add_key(
            &owner,
            KeyTypeKind::Keyring,
            "duplicate".to_string(),
            Vec::new(),
            KEY_SPEC_THREAD_KEYRING,
        )
        .unwrap() as i32;
    let second = manager
        .add_key(
            &owner,
            KeyTypeKind::Keyring,
            "duplicate".to_string(),
            Vec::new(),
            KEY_SPEC_PROCESS_KEYRING,
        )
        .unwrap() as i32;
    assert_ne!(first, second);
    for serial in [first, second] {
        manager.keys.get_mut(&serial).unwrap().perm = permission_mask(
            KeyPermission::ALL,
            KeyPermission::VIEW | KeyPermission::SEARCH,
        );
    }
    assert!(
        manager.keys[&first].published_name.unwrap().order
            < manager.keys[&second].published_name.unwrap().order
    );
    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &owner,
                    KeyctlCommand::JoinSession {
                        name: Some("duplicate".to_string()),
                    },
                )
                .unwrap(),
        ) as i32,
        first
    );

    manager.revoke_key(first).unwrap();
    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &owner,
                    KeyctlCommand::JoinSession {
                        name: Some("duplicate".to_string()),
                    },
                )
                .unwrap(),
        ) as i32,
        second
    );

    let key_count = manager.keys.len();
    let private = manager
        .add_key(
            &owner,
            KeyTypeKind::Keyring,
            ".private".to_string(),
            Vec::new(),
            KEY_SPEC_THREAD_KEYRING,
        )
        .unwrap() as i32;
    assert_eq!(manager.keys[&private].published_name, None);
    assert_eq!(manager.keys.len(), key_count + 1);

    manager.next_name_order = u64::MAX;
    let key_count = manager.keys.len();
    let links_before = manager.keys[&manager.thread_keyrings[&owner.thread_owner]]
        .links
        .clone();
    assert_eq!(
        manager.add_key(
            &owner,
            KeyTypeKind::Keyring,
            "order-exhausted".to_string(),
            Vec::new(),
            KEY_SPEC_THREAD_KEYRING,
        ),
        Err(LinuxError::ENOSPC.into())
    );
    assert_eq!(
        manager.keys[&manager.thread_keyrings[&owner.thread_owner]].links,
        links_before
    );
    assert_eq!(manager.keys.len(), key_count);
    assert_accounting_consistent(&manager);
}

#[test]
fn persistent_lookup_carries_possession_into_link_and_refreshes_expiry() {
    let owner = actor(3, 3, 1000, 1000);
    let mut manager = KeyManager::new();
    let destination = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
        .unwrap();

    let persistent = manager
        .get_persistent_keyring(owner.real_uid(), &owner)
        .unwrap();
    assert_eq!(persistent.possession, PossessionContext::Fixed(true));
    assert!(!manager.is_possessed(persistent.serial, &owner));
    assert_eq!(manager.keys[&persistent.serial].expires_at, None);

    manager.budget.limits.link_bytes = 0;
    assert_eq!(
        manager.link_persistent_keyring(ResolvedKey::numeric(destination), persistent, &owner,),
        Err(AxError::NoMemory)
    );
    assert_eq!(manager.keys[&persistent.serial].expires_at, None);
    assert!(manager.keys[&destination].links.is_empty());

    manager.budget.limits.link_bytes = MANAGER_MAX_LINK_BYTES;
    manager
        .link_persistent_keyring(ResolvedKey::numeric(destination), persistent, &owner)
        .unwrap();
    let first_expiry = manager.keys[&persistent.serial].expires_at.unwrap();
    assert!(first_expiry > wall_time().as_secs());
    assert_eq!(manager.keys[&destination].links, [persistent.serial]);
    assert_eq!(manager.keys[&persistent.serial].root_refs, 1);
    assert_eq!(manager.keys[&persistent.serial].link_refs, 1);

    let reused = manager
        .get_persistent_keyring(owner.real_uid(), &owner)
        .unwrap();
    assert_eq!(reused.serial, persistent.serial);
    assert!(manager.keys[&persistent.serial].expires_at.unwrap() >= first_expiry);

    manager.keys.get_mut(&persistent.serial).unwrap().expires_at = Some(0);
    let replacement = manager
        .get_persistent_keyring(owner.real_uid(), &owner)
        .unwrap();
    assert_ne!(replacement.serial, persistent.serial);
    assert_eq!(manager.keys[&persistent.serial].root_refs, 0);
    assert_eq!(manager.keys[&persistent.serial].link_refs, 1);
    assert_eq!(manager.keys[&replacement.serial].expires_at, None);
    manager
        .link_persistent_keyring(ResolvedKey::numeric(destination), replacement, &owner)
        .unwrap();
    assert!(!manager.keys.contains_key(&persistent.serial));
    assert_eq!(manager.keys[&destination].links, [replacement.serial]);
    assert!(manager.keys[&replacement.serial].expires_at.is_some());
    assert_accounting_consistent(&manager);
}

#[test]
fn layer_two_rejects_invalid_type_payloads() {
    let owner = actor(1, 1, 1000, 1000);
    assert!(matches!(
        Key::positive(
            KeyTypeKind::BigKey,
            "empty-big-key".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ),
        Err(AxError::InvalidInput)
    ));
    assert!(matches!(
        Key::positive(
            KeyTypeKind::User,
            "empty-user".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ),
        Err(AxError::InvalidInput)
    ));
    assert!(matches!(
        Key::positive(
            KeyTypeKind::Logon,
            "service:empty".to_string(),
            Vec::new(),
            owner.owner_uid(),
            owner.owner_gid(),
        ),
        Err(AxError::InvalidInput)
    ));
    assert!(matches!(
        Key::positive(
            KeyTypeKind::Logon,
            "missing-separator".to_string(),
            vec![1],
            owner.owner_uid(),
            owner.owner_gid(),
        ),
        Err(AxError::InvalidInput)
    ));
    assert!(matches!(
        Key::positive(
            KeyTypeKind::Logon,
            ":missing-prefix".to_string(),
            vec![1],
            owner.owner_uid(),
            owner.owner_gid(),
        ),
        Err(AxError::InvalidInput)
    ));
    assert!(matches!(
        Key::new(
            KeyTypeKind::Keyring,
            "ring".to_string(),
            vec![1],
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        ),
        Err(AxError::InvalidInput)
    ));
}

#[test]
fn staged_link_growth_is_geometric() {
    const LINKS: i32 = 4_096;

    let owner = actor(3, 3, 0, 0);
    let mut ring = Key::keyring(
        "wide".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    )
    .unwrap();
    let mut reallocations = 0;
    for serial in 1..=LINKS {
        if let Some(new_capacity) = ring.next_link_capacity().unwrap() {
            ring.links = ring.stage_link_push(serial, new_capacity).unwrap();
            reallocations += 1;
        } else {
            ring.links.push(serial);
        }
    }

    assert_eq!(ring.links.len(), LINKS as usize);
    assert!(ring.links.capacity() >= LINKS as usize);
    assert!(reallocations <= 11, "reallocated {reallocations} times");
}

#[test]
fn payload_update_rejects_empty_user_and_logon_data() {
    let owner = actor(4, 4, 1000, 1000);
    let mut manager = KeyManager::new();
    for (kind, description) in [
        (KeyTypeKind::User, "user"),
        (KeyTypeKind::Logon, "service:secret"),
    ] {
        let serial = manager.insert_key(Key::positive(
            kind,
            description.to_string(),
            vec![0xa5],
            owner.owner_uid(),
            owner.owner_gid(),
        ));
        assert_eq!(
            manager.replace_payload(serial, Vec::new()),
            Err(AxError::InvalidInput)
        );
        assert_eq!(manager.keys[&serial].payload, vec![0xa5]);
    }
    assert_accounting_consistent(&manager);
}

#[test]
fn guessed_serial_does_not_receive_possessor_permissions() {
    let owner = actor(1, 1, 1000, 1000);
    let outsider = actor(2, 2, 2000, 2000);
    let same_uid = actor(3, 3, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "secret".to_string(),
        Vec::from([1, 2, 3]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, serial).unwrap();

    assert!(
        manager
            .key_has_perm(serial, &owner, KeyPermission::READ)
            .unwrap()
    );
    assert!(
        !manager
            .key_has_perm(serial, &same_uid, KeyPermission::READ)
            .unwrap()
    );
    assert!(
        !manager
            .key_has_perm(serial, &outsider, KeyPermission::READ)
            .unwrap()
    );
}

#[test]
fn possessor_lane_requires_a_searchable_path_to_the_exact_key() {
    let owner = actor(11, 11, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "possessor-only".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.keys.get_mut(&serial).unwrap().perm =
        KeyPermissionMask::try_from_raw(0x0a00_0000).unwrap();
    manager.link_key_replace(root, serial).unwrap();

    assert!(manager.is_possessed(serial, &owner));
    assert!(
        manager
            .key_has_perm(serial, &owner, KeyPermission::READ)
            .unwrap()
    );

    manager.keys.get_mut(&serial).unwrap().perm =
        KeyPermissionMask::try_from_raw(0x0200_0000).unwrap();
    assert!(!manager.is_possessed(serial, &owner));
    assert!(
        !manager
            .key_has_perm(serial, &owner, KeyPermission::READ)
            .unwrap()
    );
}

#[test]
fn keyring_cycles_are_rejected_and_traversal_depth_is_bounded() {
    let owner = actor(21, 21, 1000, 1000);
    let mut manager = KeyManager::new();

    let cycle_a = manager.create_keyring(
        "cycle-a".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let cycle_b = manager.create_keyring(
        "cycle-b".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    manager.link_key_replace(cycle_a, cycle_b).unwrap();
    assert_eq!(
        manager.link_key_replace(cycle_b, cycle_a),
        Err(LinuxError::EDEADLK.into())
    );

    let mut rings = Vec::new();
    for index in 0..=KEYRING_SEARCH_MAX_DEPTH + 1 {
        rings.push(manager.create_keyring(
            format!("ring-{index}"),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        ));
    }
    manager.thread_keyrings.insert(owner.thread_owner, rings[0]);
    for pair in rings.windows(2) {
        manager.link_key_replace(pair[0], pair[1]).unwrap();
    }
    let beyond_search_depth = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "beyond-search-depth".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager
        .link_key_replace(*rings.last().unwrap(), beyond_search_depth)
        .unwrap();

    assert!(manager.is_possessed(*rings.last().unwrap(), &owner));
    assert!(!manager.is_possessed(beyond_search_depth, &owner));
    assert!(
        manager
            .search_keyring(
                rings[0],
                &owner,
                KeyTypeKind::User,
                "beyond-search-depth",
                &mut BTreeSet::new(),
            )
            .unwrap()
            .is_none()
    );
    let destination = manager.create_keyring(
        "depth-destination".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    assert!(manager.link_key_replace(destination, rings[0]) == Err(LinuxError::ELOOP.into()));
}

#[test]
fn link_depth_counts_only_nested_keyrings() {
    let owner = actor(26, 26, 1000, 1000);
    let mut manager = KeyManager::new();
    let mut rings = Vec::new();
    for index in 0..=KEYRING_SEARCH_MAX_DEPTH {
        rings.push(manager.create_keyring(
            format!("bounded-ring-{index}"),
            owner.owner_uid(),
            owner.owner_gid(),
            thread_process_keyring_permissions(),
        ));
    }
    for pair in rings.windows(2) {
        manager.link_key_replace(pair[0], pair[1]).unwrap();
    }
    let ordinary_key = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "ordinary-leaf".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager
        .link_key_replace(*rings.last().unwrap(), ordinary_key)
        .unwrap();

    let valid_destination = manager.create_keyring(
        "valid-depth-destination".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    manager
        .link_key_replace(valid_destination, rings[0])
        .unwrap();

    let nested_keyring = manager.create_keyring(
        "one-keyring-too-deep".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    manager
        .link_key_replace(*rings.last().unwrap(), nested_keyring)
        .unwrap();
    let invalid_destination = manager.create_keyring(
        "invalid-depth-destination".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    assert_eq!(
        manager.link_key_replace(invalid_destination, rings[0]),
        Err(LinuxError::ELOOP.into())
    );
}

#[test]
fn an_existing_request_result_links_only_to_an_explicit_destination() {
    let owner = actor(31, 31, 1000, 1000);
    let mut manager = KeyManager::new();
    let search_root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let destination = manager.create_keyring(
        "request-destination".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "existing".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(search_root, destination).unwrap();
    manager.link_key_replace(search_root, serial).unwrap();
    let namespace = owner.user_ns.identity();

    manager
        .link_existing_request_result(0, serial, &owner, namespace)
        .unwrap();
    assert!(!manager.keys[&destination].links.contains(&serial));

    manager
        .link_existing_request_result(destination, serial, &owner, namespace)
        .unwrap();
    assert!(manager.keys[&destination].links.contains(&serial));
}

#[test]
fn logon_payload_is_not_userspace_readable_even_when_possessed() {
    let owner = actor(41, 41, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::Logon,
        "service:secret".to_string(),
        Vec::from([1, 2, 3]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, serial).unwrap();

    assert!(!KeyTypeKind::Logon.userspace_readable());
    assert!(manager.is_possessed(serial, &owner));
    assert!(
        !manager
            .key_has_perm(serial, &owner, KeyPermission::READ)
            .unwrap()
    );
}

#[test]
fn visible_tid_rebinding_keeps_the_immutable_thread_owner() {
    let first = actor(51, 51, 1000, 1000);
    let mut manager = KeyManager::new();
    let thread_ring = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &first, true)
        .unwrap();

    let mut rebound = first.clone();
    rebound.tid = 1;
    assert_eq!(
        manager.special_keyring(KEY_SPEC_THREAD_KEYRING, &rebound, false),
        Ok(thread_ring)
    );
    assert!(manager.is_possessed(thread_ring, &rebound));
}

#[test]
fn restriction_and_failed_move_preserve_the_source_link() {
    let owner = actor(61, 61, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let source = manager.create_keyring(
        "source".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let destination = manager.create_keyring(
        "destination".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "movable".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, source).unwrap();
    manager.link_key_replace(root, destination).unwrap();
    manager.link_key_replace(source, serial).unwrap();
    manager.keys.get_mut(&destination).unwrap().restricted = true;

    assert_eq!(
        manager.move_key_link(source, destination, serial, false),
        Err(AxError::OperationNotPermitted)
    );
    assert!(manager.keys[&source].links.contains(&serial));
    assert!(!manager.keys[&destination].links.contains(&serial));
}

#[test]
fn move_replaces_an_existing_match_without_growing_the_destination() {
    let owner = actor(66, 66, 1000, 1000);
    let mut manager = KeyManager::new();
    let source = manager.create_keyring(
        "source".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let destination = manager.create_keyring(
        "destination".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let moved = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "same-description".to_string(),
        Vec::from([1]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    let replaced = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "same-description".to_string(),
        Vec::from([2]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(source, moved).unwrap();
    manager.link_key_replace(destination, replaced).unwrap();
    let destination_len = manager.keys[&destination].links.len();

    manager
        .move_key_link(source, destination, moved, false)
        .unwrap();

    assert!(!manager.keys[&source].links.contains(&moved));
    assert_eq!(manager.keys[&destination].links.len(), destination_len);
    assert!(manager.keys[&destination].links.contains(&moved));
    assert!(!manager.keys[&destination].links.contains(&replaced));
}

#[test]
fn payload_quota_failure_is_non_mutating() {
    let owner = actor(71, 71, 1000, 1000);
    let mut manager = KeyManager::new();
    let fixed_charge = "quota".len() + 1;
    let original_len = KEY_MAXBYTES_DEFAULT - fixed_charge - 1;
    let original = vec![0x5a; original_len];
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "quota".to_string(),
        original.clone(),
        owner.owner_uid(),
        owner.owner_gid(),
    ));

    assert_eq!(
        manager.replace_payload(serial, vec![0xa5; original_len + 2]),
        Err(LinuxError::EDQUOT.into())
    );
    assert_eq!(manager.keys[&serial].payload, original);
}

#[test]
fn big_key_update_checks_transient_raw_payload_quota() {
    let owner = actor(72, 72, 1000, 1000);
    let mut manager = KeyManager::new();
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::BigKey,
        "big".to_string(),
        vec![0x11],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    let current = manager.owners.usage(owner.owner_uid()).bytes;
    let filler_fixed_charge = "filler".len() + 1;
    let spare = 32;
    let filler_len = KEY_MAXBYTES_DEFAULT - current - filler_fixed_charge - spare;
    manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "filler".to_string(),
        vec![0; filler_len],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    let before = manager.owners.usage(owner.owner_uid());

    assert_eq!(
        manager.replace_payload(serial, vec![0x22; 64]),
        Err(LinuxError::EDQUOT.into())
    );
    assert_eq!(manager.keys[&serial].payload, vec![0x11]);
    assert_eq!(manager.owners.usage(owner.owner_uid()), before);
    assert_accounting_consistent(&manager);
}

#[test]
fn link_quota_failure_is_non_mutating() {
    let owner = actor(73, 73, 1000, 1000);
    let mut manager = KeyManager::new();
    let destination = manager.create_keyring(
        "quota-ring".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "link-target".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    let filler_fixed_charge = "filler".len() + 1;
    let current_charge = manager.owners.usage(owner.owner_uid()).bytes;
    let filler_len =
        KEY_MAXBYTES_DEFAULT - current_charge - filler_fixed_charge - (KEY_LINK_CHARGE - 1);
    manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "filler".to_string(),
        vec![0; filler_len],
        owner.owner_uid(),
        owner.owner_gid(),
    ));

    assert_eq!(
        manager.link_key_replace(destination, serial),
        Err(LinuxError::EDQUOT.into())
    );
    assert!(manager.keys[&destination].links.is_empty());
}

#[test]
fn abi_quota_and_resident_budget_use_distinct_charges() {
    let owner = actor(74, 74, 1000, 1000);
    let mut description = String::with_capacity(128);
    description.push_str("large");
    let mut payload = Vec::with_capacity(4096);
    payload.resize(1024, 0x5a);
    let key = Key::positive(
        KeyTypeKind::BigKey,
        description,
        payload,
        owner.owner_uid(),
        owner.owner_gid(),
    )
    .unwrap();

    assert_eq!(KeyTypeKind::BigKey.payload_limit(), 1 << 20);
    assert_eq!(
        key.abi_charge,
        AbiQuotaCharge {
            keys: 1,
            bytes: "large".len() + 1 + BIG_KEY_ABI_PAYLOAD_CHARGE,
        }
    );
    assert_eq!(
        key.resident_charge.bytes,
        size_of::<Key>() + KEY_RESIDENT_NODE_OVERHEAD + 128 + 4096
    );
    assert!(key.resident_charge.bytes > key.abi_charge.bytes);
}

#[test]
fn quota_overrun_only_relaxes_creation_admission() {
    let owner = actor(75, 75, 1000, 1000);
    let mut manager = KeyManager::new();
    let oversized = "x".repeat(KEY_MAXBYTES_DEFAULT + 1);
    let ring = manager
        .try_insert_key(
            Key::keyring(
                oversized,
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            )
            .unwrap(),
            QuotaAdmission::AllowOverrun,
        )
        .unwrap();
    let target = manager
        .try_insert_key(
            Key::positive(
                KeyTypeKind::User,
                "exempt-target".to_string(),
                vec![0],
                owner.owner_uid(),
                owner.owner_gid(),
            )
            .unwrap(),
            QuotaAdmission::Exempt,
        )
        .unwrap();
    let usage_before = manager.owners.usage(owner.owner_uid());
    let budget_before = manager.budget.used;

    assert!(usage_before.bytes > KEY_MAXBYTES_DEFAULT);
    assert_eq!(
        manager.link_key_replace(ring, target),
        Err(LinuxError::EDQUOT.into())
    );
    assert_eq!(manager.owners.usage(owner.owner_uid()), usage_before);
    assert_eq!(manager.budget.used, budget_before);
    assert!(manager.keys[&ring].links.is_empty());
    assert_eq!(manager.keys[&target].link_refs, 0);
    assert_accounting_consistent(&manager);
}

#[test]
fn quota_refunds_succeed_while_usage_remains_over_limit() {
    let owner = actor(76, 76, 1000, 1000);
    let mut manager = KeyManager::new();
    let oversized = manager
        .try_insert_key(
            Key::keyring(
                "x".repeat(KEY_MAXBYTES_DEFAULT + 1),
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            )
            .unwrap(),
            QuotaAdmission::AllowOverrun,
        )
        .unwrap();
    let removable = manager
        .try_insert_key(
            Key::positive(
                KeyTypeKind::User,
                "removable".to_string(),
                vec![1],
                owner.owner_uid(),
                owner.owner_gid(),
            )
            .unwrap(),
            QuotaAdmission::AllowOverrun,
        )
        .unwrap();

    manager.discard_new_key(removable).unwrap();
    assert!(manager.keys.contains_key(&oversized));
    assert!(!manager.keys.contains_key(&removable));
    assert!(manager.owners.usage(owner.owner_uid()).bytes > KEY_MAXBYTES_DEFAULT);
    assert_accounting_consistent(&manager);
}

#[test]
fn manager_budget_failure_preserves_serial_and_ledgers() {
    let owner = actor(77, 77, 1000, 1000);
    let mut manager = KeyManager::with_budget(ManagerBudgetLimits {
        objects: 0,
        bytes: usize::MAX,
        link_bytes: usize::MAX,
    });

    assert_eq!(
        manager.try_insert_key(
            Key::positive(
                KeyTypeKind::User,
                "budget".to_string(),
                vec![1, 2, 3],
                owner.owner_uid(),
                owner.owner_gid(),
            )
            .unwrap(),
            QuotaAdmission::Enforced,
        ),
        Err(AxError::NoMemory)
    );
    assert_eq!(manager.next_serial, 1);
    assert!(manager.keys.is_empty());
    assert!(manager.owners.usage.is_empty());
    assert_eq!(manager.budget.used, ManagerBudgetUsage::default());
    assert_accounting_consistent(&manager);
}

#[test]
fn manager_budget_accounts_staged_link_growth_at_peak() {
    let limits = ManagerBudgetLimits {
        objects: usize::MAX,
        bytes: usize::MAX,
        link_bytes: 40,
    };
    let mut budget = ManagerBudget::new(limits);
    budget.used.link_bytes = 16;

    assert_eq!(
        budget.plan_replace(
            ResidentCharge {
                objects: 0,
                bytes: 0,
                link_bytes: 16,
            },
            ResidentCharge {
                objects: 0,
                bytes: 0,
                link_bytes: 32,
            },
        ),
        Ok(ManagerBudgetUsage {
            objects: 0,
            bytes: 0,
            link_bytes: 32,
        })
    );
    assert_eq!(
        budget.check_transient(ResidentCharge {
            objects: 0,
            bytes: 0,
            link_bytes: 32,
        }),
        Err(AxError::NoMemory)
    );
    assert_eq!(budget.used.link_bytes, 16);
}

#[test]
fn key_user_records_separate_live_and_quota_counts() {
    let owner = actor(77, 77, 1000, 1000);
    let mut manager = KeyManager::new();
    manager
        .try_insert_key(
            Key::positive(
                KeyTypeKind::User,
                "charged".to_string(),
                vec![1],
                owner.owner_uid(),
                owner.owner_gid(),
            )
            .unwrap(),
            QuotaAdmission::Enforced,
        )
        .unwrap();
    manager
        .try_insert_key(
            Key::keyring(
                "exempt".to_string(),
                owner.owner_uid(),
                owner.owner_gid(),
                persistent_keyring_permissions(),
            )
            .unwrap(),
            QuotaAdmission::Exempt,
        )
        .unwrap();

    let records = manager.key_user_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].uid.into_raw(), 1000);
    assert_eq!(records[0].usage, 2);
    assert_eq!(records[0].keys, 2);
    assert_eq!(records[0].instantiated_keys, 2);
    assert_eq!(records[0].quota_keys, 1);
    assert_eq!(records[0].quota_bytes, "charged".len() + 1 + 1);
}

#[test]
fn visible_uid_and_quota_uid_diverge_until_chown_transfers_both() {
    let mut owner = actor(78, 78, 1000, 1000);
    owner.has_sys_admin = true;
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "quota-owner".to_string(),
        vec![1],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, serial).unwrap();

    let visible_uid = Kuid::from_raw(2000).unwrap();
    manager.keys.get_mut(&serial).unwrap().uid = visible_uid;
    manager.replace_payload(serial, vec![2; 32]).unwrap();
    assert_eq!(manager.keys[&serial].uid, visible_uid);
    assert_eq!(manager.keys[&serial].quota_uid, owner.owner_uid());
    assert!(
        manager
            .key_user_records()
            .unwrap()
            .iter()
            .all(|record| record.uid != visible_uid)
    );

    let charge = manager.keys[&serial].abi_charge;
    let old_usage = manager.owners.usage(owner.owner_uid());
    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &owner,
                    KeyctlCommand::Chown {
                        key: serial,
                        uid: Some(visible_uid.into_raw()),
                        gid: None,
                    },
                )
                .unwrap(),
        ),
        0
    );
    assert_eq!(manager.keys[&serial].uid, visible_uid);
    assert_eq!(manager.keys[&serial].quota_uid, owner.owner_uid());
    assert_eq!(manager.owners.usage(owner.owner_uid()), old_usage);
    assert_eq!(manager.owners.usage(visible_uid), OwnerUsage::default());

    assert_eq!(
        keyctl_value(
            manager
                .keyctl(
                    &owner,
                    KeyctlCommand::Chown {
                        key: serial,
                        uid: Some(3000),
                        gid: None,
                    },
                )
                .unwrap(),
        ),
        0
    );
    let new_uid = Kuid::from_raw(3000).unwrap();
    assert_eq!(manager.keys[&serial].uid, new_uid);
    assert_eq!(manager.keys[&serial].quota_uid, new_uid);
    assert_eq!(
        manager.owners.usage(owner.owner_uid()).keys,
        old_usage.keys - charge.keys
    );
    assert_eq!(
        manager.owners.usage(owner.owner_uid()).bytes,
        old_usage.bytes - charge.bytes
    );
    assert_eq!(manager.owners.usage(new_uid).keys, charge.keys);
    assert_eq!(manager.owners.usage(new_uid).bytes, charge.bytes);

    manager.unlink_key_from_keyring(root, serial).unwrap();
    assert_eq!(manager.owners.usage(new_uid), OwnerUsage::default());
    assert_accounting_consistent(&manager);
}

#[test]
fn owner_quota_is_per_kuid_not_a_global_object_limit() {
    let nonroot = actor(78, 78, 1000, 1000);
    let root = actor(79, 79, 0, 0);
    let mut manager = KeyManager::new();

    for index in 0..KEY_MAXKEYS_DEFAULT {
        manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    format!("nonroot-{index}"),
                    vec![0],
                    nonroot.owner_uid(),
                    nonroot.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::Enforced,
            )
            .unwrap();
    }
    assert_eq!(
        manager.try_insert_key(
            Key::positive(
                KeyTypeKind::User,
                "nonroot-over-limit".to_string(),
                vec![1],
                nonroot.owner_uid(),
                nonroot.owner_gid(),
            )
            .unwrap(),
            QuotaAdmission::Enforced,
        ),
        Err(LinuxError::EDQUOT.into())
    );
    for description in ["root-a", "root-b"] {
        manager
            .try_insert_key(
                Key::positive(
                    KeyTypeKind::User,
                    description.to_string(),
                    vec![0],
                    root.owner_uid(),
                    root.owner_gid(),
                )
                .unwrap(),
                QuotaAdmission::Enforced,
            )
            .unwrap();
    }
    assert_eq!(
        manager.owners.usage(nonroot.owner_uid()).keys,
        KEY_MAXKEYS_DEFAULT
    );
    assert_eq!(manager.owners.usage(root.owner_uid()).keys, 2);
    assert_accounting_consistent(&manager);
}

#[test]
fn unlink_collects_only_after_the_last_reference() {
    let owner = actor(80, 80, 1000, 1000);
    let mut manager = KeyManager::new();
    let first = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let second = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
        .unwrap();
    let serial = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "shared".to_string(),
        vec![0x5a; 32],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(first, serial).unwrap();
    manager.link_key_replace(second, serial).unwrap();
    assert_eq!(manager.keys[&serial].link_refs, 2);

    manager.unlink_key_from_keyring(first, serial).unwrap();
    assert!(manager.keys.contains_key(&serial));
    assert_eq!(manager.keys[&serial].link_refs, 1);
    manager.unlink_key_from_keyring(second, serial).unwrap();
    assert!(!manager.keys.contains_key(&serial));
    assert_accounting_consistent(&manager);

    assert!(manager.keys[&first].links.capacity() != 0);
    assert!(manager.budget.used.link_bytes != 0);
    manager.clear_keyring_links(first).unwrap();
    manager.clear_keyring_links(second).unwrap();
    assert_eq!(manager.keys[&first].links.capacity(), 0);
    assert_eq!(manager.keys[&second].links.capacity(), 0);
    assert_eq!(manager.budget.used.link_bytes, 0);
    assert_accounting_consistent(&manager);
}

#[test]
fn prepared_task_gc_retires_a_deep_keyring_chain_without_stack_growth() {
    const DEPTH: usize = 2_048;

    let owner = actor(80, 80, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .try_insert_key(
            Key::keyring(
                "r0".to_string(),
                owner.owner_uid(),
                owner.owner_gid(),
                thread_process_keyring_permissions(),
            )
            .unwrap(),
            QuotaAdmission::Exempt,
        )
        .unwrap();
    manager.install_root(RootSource::Thread(80), root).unwrap();
    let mut parent = root;
    for index in 1..DEPTH {
        let child = manager
            .try_insert_key(
                Key::keyring(
                    format!("r{index}"),
                    owner.owner_uid(),
                    owner.owner_gid(),
                    thread_process_keyring_permissions(),
                )
                .unwrap(),
                QuotaAdmission::Exempt,
            )
            .unwrap();
        manager.link_key_replace(parent, child).unwrap();
        parent = child;
    }

    manager
        .exec_committed(KeyTaskOwner::new(owner.thread_owner, owner.process_owner))
        .unwrap();
    assert!(manager.keys.is_empty());
    assert_accounting_consistent(&manager);
}

#[test]
fn invalidate_validates_root_accounting_before_detaching_roots() {
    let owner = actor(81, 81, 1000, 1000);
    let mut manager = KeyManager::new();
    let ring = manager.insert_key(Key::keyring(
        "multi-root".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    ));
    manager.install_root(RootSource::Thread(81), ring).unwrap();
    manager.install_root(RootSource::Session(81), ring).unwrap();
    let parent = manager.insert_key(Key::keyring(
        "parent".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    ));
    manager
        .install_root(RootSource::Process(81), parent)
        .unwrap();
    manager.link_key_replace(parent, ring).unwrap();
    manager.keys.get_mut(&ring).unwrap().root_refs = 1;
    let thread_roots = manager.thread_keyrings.clone();
    let session_roots = manager.session_keyrings.clone();
    let parent_links = manager.keys[&parent].links.clone();

    assert_eq!(manager.remove_key_everywhere(ring), Err(AxError::BadState));
    assert_eq!(manager.thread_keyrings, thread_roots);
    assert_eq!(manager.session_keyrings, session_roots);
    assert_eq!(manager.keys[&parent].links, parent_links);
    assert!(manager.keys.contains_key(&ring));
}

#[test]
fn revoke_releases_payload_links_and_recursive_children() {
    let owner = actor(82, 82, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let ring = manager.insert_key(Key::keyring(
        "revoked-ring".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    ));
    let child = manager.insert_key(Key::positive(
        KeyTypeKind::Logon,
        "service:secret".to_string(),
        vec![0xa5; 64],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, ring).unwrap();
    manager.link_key_replace(ring, child).unwrap();
    let ring_base_bytes = manager.keys[&ring].abi_charge.bytes - KEY_LINK_CHARGE;

    manager.revoke_key(ring).unwrap();
    assert_eq!(manager.keys[&ring].state, KeyState::Revoked);
    assert!(manager.keys[&ring].links.is_empty());
    assert_eq!(manager.keys[&ring].abi_charge.bytes, ring_base_bytes);
    assert!(!manager.keys.contains_key(&child));
    assert_accounting_consistent(&manager);
}

#[test]
fn same_owner_move_reserves_destination_quota_before_source_refund() {
    let owner = actor(84, 84, 1000, 1000);
    let mut manager = KeyManager::new();
    let source = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let destination = manager
        .special_keyring(KEY_SPEC_PROCESS_KEYRING, &owner, true)
        .unwrap();
    let moved = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "moved".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(source, moved).unwrap();

    let current = manager.owners.usage(owner.owner_uid()).bytes;
    let filler_base = "move-filler".len() + 1;
    let filler_len = KEY_MAXBYTES_DEFAULT - current - filler_base;
    manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "move-filler".to_string(),
        vec![0; filler_len],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    assert_eq!(
        manager.owners.usage(owner.owner_uid()).bytes,
        KEY_MAXBYTES_DEFAULT
    );
    let owners_before = manager.owners.usage.clone();
    let budget_before = manager.budget.used;

    assert_eq!(
        manager.move_key_link(source, destination, moved, false),
        Err(LinuxError::EDQUOT.into())
    );
    assert!(manager.keys[&source].links.contains(&moved));
    assert!(!manager.keys[&destination].links.contains(&moved));
    assert_eq!(manager.keys[&moved].link_refs, 1);
    assert_eq!(manager.owners.usage, owners_before);
    assert_eq!(manager.budget.used, budget_before);
    assert_accounting_consistent(&manager);
}

#[test]
fn owner_quota_overflow_is_edquot() {
    let owner = actor(85, 85, 1000, 1000);
    let uid = owner.owner_uid();
    let mut ledger = OwnerLedger::default();
    ledger.set_usage_for_test(
        uid,
        OwnerUsage {
            keys: usize::MAX,
            bytes: usize::MAX,
        },
    );
    assert_eq!(
        ledger.plan_replace(
            uid,
            QuotaAdmission::Enforced,
            AbiQuotaCharge::ZERO,
            AbiQuotaCharge { keys: 1, bytes: 1 },
        ),
        Err(LinuxError::EDQUOT.into())
    );
}

#[test]
fn serial_wrap_finds_a_hole_and_never_overwrites_a_key() {
    let owner = actor(76, 76, 1000, 1000);
    let mut manager = KeyManager::new();
    let first = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "first".to_string(),
        vec![0],
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    assert_eq!(first, 1);

    manager.next_serial = i32::MAX;
    let last = manager
        .try_insert_key(
            Key::positive(
                KeyTypeKind::User,
                "last".to_string(),
                vec![0],
                owner.owner_uid(),
                owner.owner_gid(),
            )
            .unwrap(),
            QuotaAdmission::AllowOverrun,
        )
        .unwrap();
    assert_eq!(last, i32::MAX);

    let wrapped = manager
        .try_insert_key(
            Key::positive(
                KeyTypeKind::User,
                "wrapped".to_string(),
                vec![0],
                owner.owner_uid(),
                owner.owner_gid(),
            )
            .unwrap(),
            QuotaAdmission::AllowOverrun,
        )
        .unwrap();
    assert_eq!(wrapped, 2);
    assert_eq!(manager.keys[&first].description, "first");
    assert_eq!(manager.keys[&last].description, "last");
    assert_eq!(manager.keys[&wrapped].description, "wrapped");
}

#[test]
fn search_is_breadth_first_across_keyring_links() {
    let owner = actor(81, 81, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let branch = manager.create_keyring(
        "deep-branch".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let shallow_branch = manager.create_keyring(
        "shallow-branch".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let deeper = manager.create_keyring(
        "deeper".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let deep_match = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "target".to_string(),
        Vec::from([1]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    let shallow_match = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "target".to_string(),
        Vec::from([2]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, branch).unwrap();
    manager.link_key_replace(root, shallow_branch).unwrap();
    manager.link_key_replace(branch, deeper).unwrap();
    manager.link_key_replace(deeper, deep_match).unwrap();
    manager
        .link_key_replace(shallow_branch, shallow_match)
        .unwrap();

    assert_eq!(
        manager
            .search_keyring(
                root,
                &owner,
                KeyTypeKind::User,
                "target",
                &mut BTreeSet::new(),
            )
            .unwrap()
            .map(|key| key.serial),
        Some(shallow_match)
    );
}

#[test]
fn revoked_nested_keyring_does_not_poison_an_unrelated_miss() {
    let owner = actor(91, 91, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let revoked_child = manager.create_keyring(
        "revoked-child".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    manager.link_key_replace(root, revoked_child).unwrap();
    manager.keys.get_mut(&revoked_child).unwrap().state = KeyState::Revoked;

    assert_eq!(
        manager.search_keyring(
            root,
            &owner,
            KeyTypeKind::User,
            "missing",
            &mut BTreeSet::new(),
        ),
        Ok(None)
    );
}

#[test]
fn inaccessible_match_reports_eacces_but_does_not_hide_a_later_match() {
    let owner = actor(101, 101, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let denied = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "permission-target".to_string(),
        Vec::from([1]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.keys.get_mut(&denied).unwrap().perm = KeyPermissionMask::try_from_raw(0).unwrap();
    manager.link_key_replace(root, denied).unwrap();

    assert_eq!(
        manager.search_keyring(
            root,
            &owner,
            KeyTypeKind::User,
            "permission-target",
            &mut BTreeSet::new(),
        ),
        Err(LinuxError::EACCES.into())
    );

    let branch = manager.create_keyring(
        "valid-branch".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        thread_process_keyring_permissions(),
    );
    let allowed = manager.insert_key(Key::positive(
        KeyTypeKind::User,
        "permission-target".to_string(),
        Vec::from([2]),
        owner.owner_uid(),
        owner.owner_gid(),
    ));
    manager.link_key_replace(root, branch).unwrap();
    manager.link_key_replace(branch, allowed).unwrap();

    assert_eq!(
        manager.search_keyring(
            root,
            &owner,
            KeyTypeKind::User,
            "permission-target",
            &mut BTreeSet::new(),
        ),
        Ok(Some(ResolvedKey::possessed(allowed)))
    );
}

#[test]
fn basal_keyring_is_considered_before_its_children() {
    let owner = actor(111, 111, 1000, 1000);
    let mut manager = KeyManager::new();
    let root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();

    assert_eq!(
        manager.search_keyring(
            ResolvedKey::possessed(root),
            &owner,
            KeyTypeKind::Keyring,
            "_tid.111",
            &mut BTreeSet::new(),
        ),
        Ok(Some(ResolvedKey::possessed(root)))
    );
}

#[test]
fn direct_special_lookup_does_not_grant_numeric_possession() {
    let mut shifted = actor(121, 121, 1000, 1000);
    let groups = GroupInfo::try_new(Vec::new()).unwrap();
    shifted.dac = DacCredentialView::new(
        Kuid::INITIAL_ROOT,
        Kgid::INITIAL_ROOT,
        groups,
        [0; CAPABILITY_WORDS],
        true,
    );
    let mut manager = KeyManager::new();
    let direct = manager
        .resolve_keyring(KEY_SPEC_USER_KEYRING, &shifted, true)
        .unwrap();

    assert!(
        manager
            .key_has_perm(direct, &shifted, KeyPermission::WRITE)
            .unwrap()
    );
    assert!(
        !manager
            .key_has_perm(
                ResolvedKey::numeric(direct.serial),
                &shifted,
                KeyPermission::WRITE,
            )
            .unwrap()
    );
}

#[test]
fn explicit_unpossessed_search_does_not_borrow_an_independent_possession() {
    let owner = actor(131, 131, 1000, 1000);
    let mut manager = KeyManager::new();
    let credential_root = manager
        .special_keyring(KEY_SPEC_THREAD_KEYRING, &owner, true)
        .unwrap();
    let independently_possessed = manager.create_keyring(
        "independently-possessed".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        permission_mask(KeyPermission::SEARCH, KeyPermission::VIEW),
    );
    manager
        .link_key_replace(credential_root, independently_possessed)
        .unwrap();

    let explicit_source = manager.create_keyring(
        "explicit-source".to_string(),
        owner.owner_uid(),
        owner.owner_gid(),
        permission_mask(KeyPermission::SEARCH, KeyPermission::SEARCH),
    );
    manager
        .link_key_replace(explicit_source, independently_possessed)
        .unwrap();

    assert_eq!(
        manager.search_keyring(
            ResolvedKey::with_possession(explicit_source, false),
            &owner,
            KeyTypeKind::Keyring,
            "independently-possessed",
            &mut BTreeSet::new(),
        ),
        Err(LinuxError::EACCES.into())
    );
}

#[test]
fn encrypted_key_type_is_not_advertised_without_a_real_type_backend() {
    assert_eq!(KeyTypeKind::from_name("encrypted"), None);
}
