extern crate std;

use alloc::{boxed::Box, sync::Arc, vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::{sync::Barrier, thread, vec::Vec};

use axerrno::AxError;
use axsync::spin::SpinNoIrq;
use axtask::{
    AxCpuMask, DeadlineParameters, RequestedSlice, SchedClass, SchedState, TaskSchedulingSnapshot,
    UclampRequest, UtilizationBounds,
};
use linux_raw_sys::general::CAP_CHOWN;
use tk_linux_signal::{
    PreparedSignal, SignalInfo, SignalQueueAccount, Signo,
    api::{ProcessSignalManager, SharedSignalActions, SignalActions, ThreadSignalManager},
};

use super::{
    CgroupNamespace, Dumpability, GroupLeaderIdentityBinding, GroupLeaderSignalIdentity, Mempolicy,
    MempolicyRange, MempolicySnapshot, MempolicyState, NetworkNamespace, PID_MAX_LIMIT,
    PTRACE_REVERSE_LINK_HARD_LIMIT, PidNamespace, PidNamespacePids, PreparedPtraceReverseLink,
    ProcessAccessState, ProcessImageBinding, PtraceReverseLinkDrain, PtraceReverseLinkNode,
    PtraceReverseLinks, SIGNAL_QUEUE_GLOBAL_HARD_LIMIT, SIGNAL_QUEUE_PER_USER_HARD_LIMIT,
    TimeNamespace, UserNamespace, UtsNamespace, ZombieSchedulerSnapshot, begin_exec_control,
    coredump_image_snapshot, group_exit_handoff_requires_kill, init_uts_state,
    ptrace_image_snapshot_if_owned, ptrace_image_snapshot_if_session,
    ptrace_inactive_image_snapshot_if_session, ptrace_lifecycle_first_key,
    release_exec_control_owner, release_vfork_control_parent,
    replace_process_image_with_group_handoff, retire_group_leader_signal_owner,
    scheduler_publication_matches, scheduler_tlb_state_snapshot, snapshot_credential_image,
    snapshot_group_credential_image, try_allocate_namespace_id, try_increment_bounded,
};

#[test]
fn exec_admission_waits_for_runnable_child_publication_to_finish() {
    // Force the SMP window after runqueue publication but before the
    // parent's PendingThreadPublication::finish releases exec exclusion.
    let mut state = ExecControlState {
        pending_thread_additions: 1,
        ..ExecControlState::default()
    };
    assert_eq!(begin_exec_control(&mut state, 42), Err(AxError::WouldBlock));
    assert_eq!(state.owner, None);
    // Completing publication wakes exec_event; the retry owns the gate.
    state.pending_thread_additions -= 1;
    assert_eq!(begin_exec_control(&mut state, 42), Ok(()));
    assert_eq!(state.owner, Some(42));
    assert_eq!(
        begin_exec_control(&mut state, 43),
        Err(AxError::Interrupted)
    );
    state.group_exit = true;
    assert_eq!(
        begin_exec_control(&mut state, 42),
        Err(AxError::Interrupted)
    );
}

#[test]
fn exec_admission_rechecks_publication_completion_after_arming_waiter() {
    let event: axpoll::PollSet = axpoll::PollSet::new();
    let mut state = ExecControlState {
        pending_thread_additions: 1,
        ..ExecControlState::default()
    };
    let mut attempts = 0;
    let result = crate::readiness::block_on_poll_set(&event, || {
        attempts += 1;
        if attempts == 2 {
            // Publication completes after the first blocked attempt;
            // its wake may race the readiness waiter's installation.
            state.pending_thread_additions -= 1;
            event.wake();
        }
        begin_exec_control(&mut state, 42)
    });
    assert_eq!(result, Ok(()));
    assert_eq!(attempts, 2);
    assert_eq!(state.owner, Some(42));
}

fn scheduler_snapshot(state: SchedState, version: u64) -> TaskSchedulingSnapshot {
    TaskSchedulingSnapshot {
        state,
        reset_on_spawn: false,
        uclamp: UclampRequest::unrestricted(),
        utilization_bounds: UtilizationBounds::unrestricted(),
        requested_slice: RequestedSlice::default(),
        deadline: DeadlineParameters::default(),
        version,
    }
}
use crate::task::{
    CapabilityState, Cred, CredentialSlot, IdMap, IdMapInputExtent, Kgid, Kuid,
    creds::capability_state_for_test,
    jobctl::{
        ExecControlState, JobControlState, PtraceControlState, PtraceRelationshipOrigin,
        PtraceSession, StopKind, StopState, VforkControlState,
    },
    ops::{
        commit_exec_alias_publication_for_test, release_exec_action_then_complete,
        task_alias_lock_held,
    },
    security::{commoncap_post_commit_probe, reset_commoncap_post_commit_probe},
};

fn kuid(raw: u32) -> Kuid {
    Kuid::from_raw(raw).unwrap()
}

fn kgid(raw: u32) -> Kgid {
    Kgid::from_raw(raw).unwrap()
}

fn credential_slot(uid: u32) -> Arc<CredentialSlot> {
    let namespace = UserNamespace::try_new_root().unwrap();
    let slot = CredentialSlot::try_new(Cred::try_root(namespace).unwrap()).unwrap();
    if uid != 0 {
        let uid = kuid(uid);
        loop {
            let mut update = slot.prepare();
            update.builder.ids.ruid = uid;
            update.builder.ids.euid = uid;
            update.builder.ids.suid = uid;
            update.builder.ids.fsuid = uid;
            match update.finish() {
                Ok(prepared) => {
                    prepared.commit();
                    break;
                }
                Err(AxError::ResourceBusy) => {
                    crate::rcu::drain_credential_retire(crate::rcu::CREDENTIAL_RETIRE_CAPACITY);
                    thread::yield_now();
                }
                Err(error) => panic!("credential update failed: {error:?}"),
            }
        }
    }
    slot
}

fn reclaim_deferred_credential_owners() {
    assert_ne!(
        crate::rcu::drain_credential_retire(crate::rcu::CREDENTIAL_RETIRE_CAPACITY),
        0,
        "credential test fixture expected a reclaimable retired owner"
    );
}

fn thread_signal_manager() -> Arc<ThreadSignalManager> {
    let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let process = Arc::new(ProcessSignalManager::new(actions, 0));
    ThreadSignalManager::try_new(process).unwrap()
}

fn registered_thread_signal_manager(
    process: Arc<ProcessSignalManager>,
    tid: u32,
) -> Arc<ThreadSignalManager> {
    let thread = ThreadSignalManager::try_new(process).unwrap();
    thread.try_register(tid).unwrap().commit().unwrap();
    thread
}

fn enqueue_accounted_signal(
    thread: &ThreadSignalManager,
    signo: Signo,
    per_user: &Arc<SignalQueueAccount>,
    global: &Arc<SignalQueueAccount>,
) {
    let outcome = thread
        .try_send_signal_with(SignalInfo::new_user(signo, 0, 1, 0), |info| {
            PreparedSignal::try_accounted(info, per_user, u64::MAX, global)
        })
        .unwrap();
    assert!(outcome.published);
}

fn enqueue_accounted_process_signal(
    process: &ProcessSignalManager,
    signo: Signo,
    per_user: &Arc<SignalQueueAccount>,
    global: &Arc<SignalQueueAccount>,
) {
    let outcome = process
        .try_send_signal_with(SignalInfo::new_user(signo, 0, 1, 0), |info| {
            PreparedSignal::try_accounted(info, per_user, u64::MAX, global)
        })
        .unwrap();
    assert!(outcome.published);
}

#[test]
fn default_uts_identity_is_product_neutral() {
    let state = init_uts_state();
    assert_eq!(&state.nodename[..state.nodename_len], b"thekernel");
    assert_eq!(&state.domainname[..state.domainname_len], b"(none)");
}

#[test]
fn uts_namespace_fork_copies_state_independently() {
    let owner = UserNamespace::try_new_root().unwrap();
    let source = UtsNamespace::try_new_root(owner.clone()).unwrap();
    source.set_nodename(b"source-node").unwrap();
    source.set_domainname(b"source-domain").unwrap();
    let copy = source.try_fork(owner).unwrap();
    source.set_nodename(b"changed-source").unwrap();
    assert_eq!(copy.nodename().unwrap(), b"source-node");
    assert_eq!(copy.domainname().unwrap(), b"source-domain");
}

#[test]
fn uts_namespace_names_snapshot_contains_both_current_fields() {
    let owner = UserNamespace::try_new_root().unwrap();
    let uts = UtsNamespace::try_new_root(owner).unwrap();
    uts.set_nodename(b"snapshot-node").unwrap();
    uts.set_domainname(b"snapshot-domain").unwrap();
    let (nodename, domainname) = uts.names_snapshot();
    assert_eq!(&nodename[..b"snapshot-node".len()], b"snapshot-node");
    assert_eq!(&domainname[..b"snapshot-domain".len()], b"snapshot-domain");
}

#[test]
fn process_access_identity_and_capability_gain_lower_dumpability_and_clear_pdeath() {
    for field in 0..4 {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
        let pdeath = AtomicU32::new(9);
        let mut update = slot.prepare();
        match field {
            0 => update.builder.ids.euid = kuid(1000),
            1 => update.builder.ids.egid = kgid(1000),
            2 => update.builder.ids.fsuid = kuid(1000),
            _ => update.builder.ids.fsgid = kgid(1000),
        }
        let publication = state.publish_credential(update.finish().unwrap(), &pdeath);
        let (proposed, retirement) = publication.complete_post_commit();
        drop(proposed);
        drop(retirement);
        assert_eq!(state.dumpability(), Dumpability::NotDumpable);
        assert_eq!(pdeath.load(Ordering::Acquire), 0);
    }

    let namespace = UserNamespace::try_new_root().unwrap();
    let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
    let mut lower = slot.prepare();
    let caps = lower.builder.caps;
    lower.builder.caps = capability_state_for_test(
        [0; tk_linux_cred::CAPABILITY_WORDS],
        [0; tk_linux_cred::CAPABILITY_WORDS],
        [0; tk_linux_cred::CAPABILITY_WORDS],
        caps.bounding(),
        [0; tk_linux_cred::CAPABILITY_WORDS],
        caps.securebits(),
    );
    lower.finish().unwrap().commit();
    let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
    let pdeath = AtomicU32::new(12);
    let (word, mask) = CapabilityState::cap_mask(CAP_CHOWN).unwrap();
    let mut gain = slot.prepare();
    let caps = gain.builder.caps;
    let mut permitted = caps.permitted();
    let mut effective = caps.effective();
    permitted[word] |= mask;
    effective[word] |= mask;
    gain.builder.caps = capability_state_for_test(
        effective,
        permitted,
        caps.inheritable(),
        caps.bounding(),
        caps.ambient(),
        caps.securebits(),
    );
    let publication = state.publish_credential(gain.finish().unwrap(), &pdeath);
    let (proposed, retirement) = publication.complete_post_commit();
    drop(proposed);
    drop(retirement);
    assert_eq!(state.dumpability(), Dumpability::NotDumpable);
    assert_eq!(pdeath.load(Ordering::Acquire), 0);
}

#[test]
fn process_access_real_and_saved_id_only_changes_do_not_lower_dumpability() {
    for field in 0..4 {
        let namespace = UserNamespace::try_new_root().unwrap();
        let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
        let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
        let pdeath = AtomicU32::new(7);
        let mut update = slot.prepare();
        match field {
            0 => update.builder.ids.ruid = kuid(1000),
            1 => update.builder.ids.suid = kuid(1000),
            2 => update.builder.ids.rgid = kgid(1000),
            _ => update.builder.ids.sgid = kgid(1000),
        }
        let publication = state.publish_credential(update.finish().unwrap(), &pdeath);
        let (proposed, retirement) = publication.complete_post_commit();
        drop(proposed);
        drop(retirement);
        assert_eq!(state.dumpability(), Dumpability::UserDumpable);
        assert_eq!(pdeath.load(Ordering::Acquire), 7);
    }
}

#[test]
fn process_access_snapshot_never_pairs_new_identity_with_user_dumpable() {
    const WRITES: usize = 2_000;
    let namespace = UserNamespace::try_new_root().unwrap();
    let slot = CredentialSlot::try_new(Cred::try_root(namespace.clone()).unwrap()).unwrap();
    let initial = kuid(1000);
    let stronger = kuid(2000);
    let mut update = slot.prepare();
    update.builder.ids.ruid = initial;
    update.builder.ids.euid = initial;
    update.builder.ids.suid = initial;
    update.builder.ids.fsuid = initial;
    update.finish().unwrap().commit();

    let state = ProcessAccessState::try_new(Dumpability::UserDumpable, namespace).unwrap();
    let binding = Arc::new(spin::RwLock::new(ProcessImageBinding {
        aspace: 1usize,
        access_state: state.clone(),
    }));
    let pdeath = Arc::new(AtomicU32::new(1));
    let stronger_ready = Arc::new(Barrier::new(2));
    let stronger_sampled = Arc::new(Barrier::new(2));
    let writer = {
        let slot = slot.clone();
        let state = state.clone();
        let pdeath = pdeath.clone();
        let stronger_ready = stronger_ready.clone();
        let stronger_sampled = stronger_sampled.clone();
        thread::spawn(move || {
            for _ in 0..WRITES {
                let mut gain = slot.prepare();
                gain.builder.ids.ruid = stronger;
                gain.builder.ids.euid = stronger;
                gain.builder.ids.suid = stronger;
                gain.builder.ids.fsuid = stronger;
                let (proposed, retirement) = state
                    .publish_credential(gain.finish().unwrap(), &pdeath)
                    .complete_post_commit();
                drop(proposed);
                drop(retirement);
                reclaim_deferred_credential_owners();
                stronger_ready.wait();
                stronger_sampled.wait();

                let mut restore = slot.prepare();
                restore.builder.ids.ruid = initial;
                restore.builder.ids.euid = initial;
                restore.builder.ids.suid = initial;
                restore.builder.ids.fsuid = initial;
                let (proposed, retirement) = state
                    .publish_credential(restore.finish().unwrap(), &pdeath)
                    .complete_post_commit();
                drop(proposed);
                drop(retirement);
                reclaim_deferred_credential_owners();
                state.set_dumpability(Dumpability::UserDumpable);
            }
        })
    };

    for _ in 0..WRITES {
        stronger_ready.wait();
        let (credential, dumpability, ..) = snapshot_credential_image(&binding, &slot);
        assert_eq!(credential.ids().euid, stronger);
        assert_eq!(dumpability, Dumpability::NotDumpable);
        stronger_sampled.wait();
    }
    writer.join().unwrap();
}

#[test]
fn process_access_exact_slot_and_group_leader_view_are_distinct() {
    let leader = credential_slot(1000);
    let exact = credential_slot(2000);
    let owner = leader.current().user_ns().clone();
    let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
    let image = spin::RwLock::new(ProcessImageBinding {
        aspace: 7usize,
        access_state: state,
    });
    let group = GroupLeaderIdentityBinding::try_new(leader).unwrap();

    let (exact_cred, exact_dumpability, exact_image, _) = snapshot_credential_image(&image, &exact);
    let (leader_cred, leader_dumpability, _, leader_image, _) =
        snapshot_group_credential_image(&image, &group);
    assert_eq!(exact_cred.ids().euid, kuid(2000));
    assert_eq!(leader_cred.ids().euid, kuid(1000));
    assert_eq!(exact_dumpability, Dumpability::UserDumpable);
    assert_eq!(leader_dumpability, Dumpability::UserDumpable);
    assert_eq!(exact_image, leader_image);
}

#[test]
fn process_access_coredump_pins_only_the_coherent_dumpable_image() {
    let owner = UserNamespace::try_new_root().unwrap();
    let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner.clone()).unwrap();
    let image = spin::RwLock::new(ProcessImageBinding {
        aspace: 41usize,
        access_state: state.clone(),
    });
    assert_eq!(coredump_image_snapshot(&image), Some(41));
    state.set_dumpability(Dumpability::NotDumpable);
    assert_eq!(coredump_image_snapshot(&image), None);

    let replacement = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
    *image.write() = ProcessImageBinding {
        aspace: 42,
        access_state: replacement,
    };
    assert_eq!(coredump_image_snapshot(&image), Some(42));
}

#[test]
fn process_access_ptrace_session_and_image_pin_share_one_snapshot() {
    let owner = UserNamespace::try_new_root().unwrap();
    let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
    let image = spin::RwLock::new(ProcessImageBinding {
        aspace: 41usize,
        access_state: state,
    });
    let ptrace_ctl = SpinNoIrq::new(PtraceControlState::default());
    let ptracer_cred = credential_slot(1000).current();

    assert_eq!(ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7), None);
    let first = ptrace_ctl
        .lock()
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &ptracer_cred,
        )
        .unwrap();
    assert_eq!(ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 8), None);
    assert_eq!(
        ptrace_image_snapshot_if_session(&ptrace_ctl, &image, first),
        Some(41)
    );
    assert_eq!(
        ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7),
        Some((
            PtraceSession {
                tracer: 7,
                tracer_kernel_tid: 70,
                generation: 1
            },
            41
        ))
    );

    // A detached session cannot retain its earlier authorization. After
    // reattach, the same tracer PID observes only the newly bound image.
    let retired_first = ptrace_ctl.lock().clear_session(first);
    assert!(retired_first.is_some());
    drop(retired_first);
    image.write().aspace = 42;
    assert_eq!(ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7), None);
    let second = ptrace_ctl
        .lock()
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &ptracer_cred,
        )
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        ptrace_image_snapshot_if_session(&ptrace_ctl, &image, first),
        None
    );
    assert_eq!(
        ptrace_image_snapshot_if_owned(&ptrace_ctl, &image, 7),
        Some((
            PtraceSession {
                tracer: 7,
                tracer_kernel_tid: 70,
                generation: 2
            },
            42
        ))
    );
}

#[test]
fn process_access_ptrace_relationship_freezes_and_retires_exact_ptracer_credential() {
    let ptracer_slot = credential_slot(1000);
    let attached_credential = ptracer_slot.current();
    let attached_credential_weak = Arc::downgrade(&attached_credential);
    let ptrace_ctl = SpinNoIrq::new(PtraceControlState::default());
    let first = ptrace_ctl
        .lock()
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &attached_credential,
        )
        .unwrap();

    // A later credential publication by the ptracer must not rewrite the
    // already-published relationship's authorization provenance.
    let replacement = ptracer_slot
        .replace_fs_ids_for_test(kuid(2000), kgid(2000))
        .unwrap();
    let relationship = {
        let control = ptrace_ctl.lock();
        let relationship = control.active_relationship().unwrap();
        drop(control);
        relationship
    };
    assert_eq!(relationship.session(), first);
    assert!(Arc::ptr_eq(
        relationship.ptracer_cred(),
        &attached_credential
    ));
    assert!(!Arc::ptr_eq(relationship.ptracer_cred(), &replacement));

    drop(attached_credential);
    let retired = {
        let mut control = ptrace_ctl.lock();
        control.clear_session(first).unwrap()
    };
    assert_eq!(retired.session(), first);
    assert!(attached_credential_weak.upgrade().is_some());

    // Reattachment binds only the replacement credential.  The old owner
    // remains alive through the explicit retirement and snapshot values,
    // both of which are now outside the ptrace control spin guard.
    let second = ptrace_ctl
        .lock()
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &replacement,
        )
        .unwrap();
    let current = ptrace_ctl.lock().active_relationship().unwrap();
    assert_ne!(current.session(), first);
    assert_eq!(current.session(), second);
    assert!(Arc::ptr_eq(current.ptracer_cred(), &replacement));

    drop(relationship);
    assert!(attached_credential_weak.upgrade().is_some());
    drop(retired);
    reclaim_deferred_credential_owners();
    assert!(attached_credential_weak.upgrade().is_none());
}

#[test]
fn process_access_ptrace_traceme_stores_calling_tracee_credential_not_parent_actor() {
    let parent_credential = credential_slot(1000).current();
    let child_slot = credential_slot(2000);
    let child_at_traceme = child_slot.current();
    let ptrace_ctl = SpinNoIrq::new(PtraceControlState::default());

    // The session identifies the real parent as tracer, but Linux
    // ptrace_link(current, real_parent) records current_cred(): the child
    // which called PTRACE_TRACEME. The parent remains only the hook actor.
    let session = ptrace_ctl
        .lock()
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Traceme,
            &child_at_traceme,
        )
        .unwrap();
    let relationship = ptrace_ctl.lock().active_relationship().unwrap();
    assert_eq!(relationship.session(), session);
    assert_eq!(relationship.origin(), PtraceRelationshipOrigin::Traceme);
    assert!(Arc::ptr_eq(relationship.ptracer_cred(), &child_at_traceme));
    assert!(!Arc::ptr_eq(
        relationship.ptracer_cred(),
        &parent_credential
    ));

    let child_after_traceme = child_slot
        .replace_fs_ids_for_test(kuid(3000), kgid(3000))
        .unwrap();
    assert!(!Arc::ptr_eq(
        relationship.ptracer_cred(),
        &child_after_traceme
    ));
}

#[test]
fn process_access_ptrace_remote_image_requires_exact_inactive_session() {
    let owner = UserNamespace::try_new_root().unwrap();
    let state = ProcessAccessState::try_new(Dumpability::UserDumpable, owner).unwrap();
    let image = spin::RwLock::new(ProcessImageBinding {
        aspace: 41usize,
        access_state: state,
    });
    let ptrace_ctl = SpinNoIrq::new(PtraceControlState::default());
    let job_ctl = SpinNoIrq::new(JobControlState::default());
    let ptracer_cred = credential_slot(1000).current();

    let first = ptrace_ctl
        .lock()
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &ptracer_cred,
        )
        .unwrap();
    assert_eq!(
        ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, first),
        None
    );
    {
        let mut job = job_ctl.lock();
        job.state = StopState::Stopped;
        job.stop_kind = StopKind::Ptrace;
        job.ptrace_session = Some(first);
    }
    assert_eq!(
        ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, first),
        Some(41)
    );

    let retired_first = ptrace_ctl.lock().clear_session(first);
    assert!(retired_first.is_some());
    drop(retired_first);
    let second = ptrace_ctl
        .lock()
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &ptracer_cred,
        )
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, second),
        None
    );
    job_ctl.lock().ptrace_session = Some(second);
    assert_eq!(
        ptrace_inactive_image_snapshot_if_session(&ptrace_ctl, &job_ctl, &image, second),
        Some(41)
    );
}

#[test]
fn process_access_ptrace_generation_exhaustion_never_wraps_or_saturates() {
    let mut state = PtraceControlState::default();
    state.generation = u64::MAX;
    let ptracer_cred = credential_slot(1000).current();
    assert_eq!(
        state.try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &ptracer_cred,
        ),
        None
    );
    assert_eq!(state.active_session(), None);
    assert_eq!(state.generation, u64::MAX);
}

#[test]
fn process_access_ptrace_reverse_link_abort_and_limit_roll_back() {
    let links = SpinNoIrq::new(PtraceReverseLinks::default());
    links.lock().try_reserve().unwrap();
    let token = PreparedPtraceReverseLink {
        owner: &links,
        tracer: 7,
        tracer_kernel_tid: 70,
        node: Some(Box::new(PtraceReverseLinkNode {
            tracee: 9,
            session: PtraceSession {
                tracer: 0,
                tracer_kernel_tid: 0,
                generation: 0,
            },
            retired_relationship: None,
            next: None,
        })),
        reserved: true,
    };
    drop(token);
    assert_eq!(links.lock().reservations, 0);

    links.lock().len = PTRACE_REVERSE_LINK_HARD_LIMIT;
    assert_eq!(links.lock().try_reserve(), Err(AxError::NoMemory));
    assert_eq!(links.lock().reservations, 0);

    links.lock().len = 0;
    links.lock().closed = true;
    assert_eq!(links.lock().try_reserve(), Err(AxError::NoSuchProcess));
    assert_eq!(links.lock().reservations, 0);
}

#[test]
fn process_access_ptrace_reverse_links_drain_exact_tracer_task() {
    fn session(tracer_kernel_tid: u32, generation: u64) -> PtraceSession {
        PtraceSession {
            tracer: 7,
            tracer_kernel_tid,
            generation,
        }
    }

    let links = SpinNoIrq::new(PtraceReverseLinks {
        head: Some(Box::new(PtraceReverseLinkNode {
            tracee: 11,
            session: session(70, 1),
            retired_relationship: None,
            next: Some(Box::new(PtraceReverseLinkNode {
                tracee: 12,
                session: session(71, 2),
                retired_relationship: None,
                next: Some(Box::new(PtraceReverseLinkNode {
                    tracee: 13,
                    session: session(70, 3),
                    retired_relationship: None,
                    next: None,
                })),
            })),
        })),
        len: 3,
        reservations: 1,
        closed: false,
    });

    let drained = links.lock().drain_task(70);
    let drained: Vec<_> = PtraceReverseLinkDrain {
        next: drained,
        retained: None,
    }
    .collect();
    assert_eq!(drained.len(), 2);
    assert!(
        drained
            .iter()
            .all(|link| link.session().tracer_kernel_tid == 70)
    );

    let links = links.lock();
    assert_eq!(links.len, 1);
    assert_eq!(links.reservations, 1);
    assert!(!links.closed);
    let retained = links.head.as_ref().unwrap();
    assert_eq!(retained.tracee, 12);
    assert_eq!(retained.session.tracer_kernel_tid, 71);
    assert!(retained.next.is_none());
}

#[test]
fn process_access_ptrace_exit_drain_retains_credential_until_outer_drop_boundary() {
    let ptracer_slot = credential_slot(1000);
    let attached_credential = ptracer_slot.current();
    let attached_credential_weak = Arc::downgrade(&attached_credential);
    let mut control = PtraceControlState::default();
    let session = control
        .try_begin(
            7,
            70,
            false,
            0,
            PtraceRelationshipOrigin::Attach,
            &attached_credential,
        )
        .unwrap();
    let mut retirement = control.clear_session(session);
    assert!(retirement.is_some());

    // Remove every non-exit owner. The old credential must then live only
    // in the relationship retirement moved into the preallocated reverse
    // node below.
    let replacement = ptracer_slot
        .replace_fs_ids_for_test(kuid(2000), kgid(2000))
        .unwrap();
    drop(replacement);
    drop(attached_credential);

    let mut drain = PtraceReverseLinkDrain {
        next: Some(Box::new(PtraceReverseLinkNode {
            tracee: 9,
            session,
            retired_relationship: None,
            next: None,
        })),
        retained: None,
    };
    assert!(drain.retain_next_retirement(|link| {
        assert_eq!(link.session(), session);
        retirement.take()
    }));
    assert!(retirement.is_none());
    assert!(attached_credential_weak.upgrade().is_some());

    // do_exit performs this drop only after lifecycle and task-parent
    // guards. The drain, rather than a temporary loop local, is therefore
    // the deterministic final owner.
    drop(drain);
    reclaim_deferred_credential_owners();
    assert!(attached_credential_weak.upgrade().is_none());
}

#[test]
fn process_access_ptrace_dual_lifecycle_order_is_total() {
    assert!(ptrace_lifecycle_first_key(0x1000, 0x2000));
    assert!(!ptrace_lifecycle_first_key(0x2000, 0x1000));
    assert!(!ptrace_lifecycle_first_key(0x1000, 0x1000));
}

#[test]
fn process_access_numa_policy_snapshot_is_immutable_and_range_specific() {
    let snapshot = MempolicySnapshot {
        process_policy: Mempolicy::new(0, 1),
        ranges: vec![
            MempolicyRange {
                start: 0x1000,
                end: 0x5000,
                policy: Mempolicy::new(2, 2),
            },
            MempolicyRange {
                start: 0x2000,
                end: 0x3000,
                policy: Mempolicy::new(3, 4),
            },
        ],
    };

    assert_eq!(snapshot.policy_for_addr(0), Mempolicy::new(0, 1));
    assert_eq!(snapshot.policy_for_addr(0x1800), Mempolicy::new(2, 2));
    assert_eq!(snapshot.policy_for_addr(0x2800), Mempolicy::new(3, 4));
}

#[test]
fn mempolicy_home_node_replaces_only_the_updated_range_prefix() {
    let mut state = MempolicyState {
        process_policy: Mempolicy::new(0, 0),
        ranges: vec![MempolicyRange {
            start: 0x1000,
            end: 0x5000,
            policy: Mempolicy::new(2, 1),
        }],
    };

    let (ranges, updated, error) =
        MempolicyState::try_set_home_node_in_range(&state.ranges, 0x2000, 0x3000, 0).unwrap();
    state.ranges = ranges;
    assert!(updated);
    assert_eq!(error, None);
    assert_eq!(state.policy_for_addr(0x1000).unwrap().home_node, None);
    assert_eq!(state.policy_for_addr(0x2000).unwrap().home_node, Some(0));
    assert_eq!(state.policy_for_addr(0x3000).unwrap().home_node, None);
}

#[test]
fn mempolicy_home_node_keeps_the_updated_prefix_before_unsupported_policy() {
    let mut state = MempolicyState {
        process_policy: Mempolicy::new(0, 0),
        ranges: vec![
            MempolicyRange {
                start: 0x1000,
                end: 0x2000,
                policy: Mempolicy::new(2, 1),
            },
            MempolicyRange {
                start: 0x2000,
                end: 0x3000,
                policy: Mempolicy::new(3, 1),
            },
        ],
    };

    let (ranges, updated, error) =
        MempolicyState::try_set_home_node_in_range(&state.ranges, 0x1000, 0x3000, 0).unwrap();
    state.ranges = ranges;
    assert!(updated);
    assert_eq!(error, Some(axerrno::LinuxError::EOPNOTSUPP));
    assert_eq!(state.policy_for_addr(0x1000).unwrap().home_node, Some(0));
    assert_eq!(state.policy_for_addr(0x2000).unwrap().home_node, None);
}

#[test]
fn mbind_default_clears_the_vma_policy_range() {
    let mut state = MempolicyState {
        process_policy: Mempolicy::new(0, 0),
        ranges: vec![MempolicyRange {
            start: 0x1000,
            end: 0x4000,
            policy: Mempolicy::new(linux_raw_sys::mempolicy::MPOL_BIND as u32, 1),
        }],
    };

    state.bind_range(0x2000, 0x3000, Mempolicy::new(0, 0));

    assert!(state.policy_for_addr(0x1800).is_some());
    assert_eq!(state.policy_for_addr(0x2800), None);
    assert!(state.policy_for_addr(0x3800).is_some());
}

#[test]
fn process_access_group_leader_exec_handoff_is_image_coherent() {
    let old_slot = credential_slot(1000);
    let new_slot = credential_slot(2000);
    let executor_old = new_slot.current();
    let executor_old_weak = Arc::downgrade(&executor_old);
    let old_owner = old_slot.current().user_ns().clone();
    let new_owner = new_slot.current().user_ns().clone();
    let old_state = ProcessAccessState::try_new(Dumpability::UserDumpable, old_owner).unwrap();
    let old_state_weak = Arc::downgrade(&old_state);
    let clone_vm_peer_state = old_state.clone();
    let new_state = ProcessAccessState::try_new(Dumpability::NotDumpable, new_owner).unwrap();
    let image = Arc::new(spin::RwLock::new(ProcessImageBinding {
        aspace: 1usize,
        access_state: old_state,
    }));
    let mempolicy = MempolicyState {
        process_policy: Mempolicy::new(0, 1),
        ranges: vec![MempolicyRange {
            start: 0x1000,
            end: 0x2000,
            policy: Mempolicy::new(2, 2),
        }],
    };
    let mempolicy = SpinNoIrq::new(mempolicy);
    let reset_under_image_lock = AtomicBool::new(false);
    let group = Arc::new(GroupLeaderIdentityBinding::try_new(old_slot).unwrap());
    let old_seen = Arc::new(Barrier::new(2));
    let new_ready = Arc::new(Barrier::new(2));
    let reader = {
        let image = image.clone();
        let group = group.clone();
        let old_seen = old_seen.clone();
        let new_ready = new_ready.clone();
        thread::spawn(move || {
            let (cred, dumpability, _, aspace, _) = snapshot_group_credential_image(&image, &group);
            assert_eq!(
                (cred.ids().euid, dumpability, aspace),
                (kuid(1000), Dumpability::UserDumpable, 1)
            );
            old_seen.wait();
            new_ready.wait();
            let (cred, dumpability, _, aspace, _) = snapshot_group_credential_image(&image, &group);
            assert_eq!(
                (cred.ids().euid, dumpability, aspace),
                (kuid(3000), Dumpability::NotDumpable, 2)
            );
        })
    };

    let mut update = new_slot.prepare();
    update.builder.ids.ruid = kuid(3000);
    update.builder.ids.euid = kuid(3000);
    update.builder.ids.suid = kuid(3000);
    update.builder.ids.fsuid = kuid(3000);
    let prepared = update.finish().unwrap();
    drop(executor_old);
    reset_commoncap_post_commit_probe();
    old_seen.wait();
    let (commit, retired_image) = replace_process_image_with_group_handoff(
        &image,
        &group,
        new_slot.clone(),
        None,
        Some(prepared),
        None,
        ProcessImageBinding {
            aspace: 2,
            access_state: new_state,
        },
        || {
            assert!(image.try_read().is_none());
            mempolicy.lock().ranges.clear();
            reset_under_image_lock.store(true, Ordering::Release);
        },
    );
    let retirement = commit.complete_post_commit();
    assert_eq!(commoncap_post_commit_probe(), (1, 2000, 3000, 1 << 1));
    assert!(executor_old_weak.upgrade().is_some());
    assert!(old_state_weak.upgrade().is_some());
    assert_eq!(clone_vm_peer_state.dumpability(), Dumpability::UserDumpable);
    drop(clone_vm_peer_state);
    assert!(old_state_weak.upgrade().is_some());
    drop(retirement);
    reclaim_deferred_credential_owners();
    assert!(executor_old_weak.upgrade().is_none());
    assert!(old_state_weak.upgrade().is_some());
    drop(retired_image);
    assert!(old_state_weak.upgrade().is_none());
    assert!(reset_under_image_lock.load(Ordering::Acquire));
    assert!(mempolicy.lock().ranges.is_empty());
    let current_state = image.read().access_state.clone();
    assert_eq!(current_state.dumpability(), Dumpability::NotDumpable);
    new_ready.wait();
    reader.join().unwrap();
}

#[test]
fn scheduler_tlb_snapshot_does_not_join_image_writer_domain() {
    let owner = Arc::new(7usize);
    let owner_weak = Arc::downgrade(&owner);
    let image = spin::RwLock::new(1usize);
    let tlb = spin::RwLock::new(owner);

    // This models an exec publication after taking image_binding.write().
    // A scheduler snapshot must remain callable without recursively
    // acquiring that lock, and its Arc must pin the observed TLB owner.
    let image_writer = image.write();
    let snapshot = scheduler_tlb_state_snapshot(&tlb);
    assert_eq!(*snapshot, 7);
    let retired = core::mem::replace(&mut *tlb.write(), Arc::new(9));
    drop(retired);
    drop(image_writer);

    assert!(owner_weak.upgrade().is_some());
    drop(snapshot);
    assert!(owner_weak.upgrade().is_none());
}

// Host tests cannot construct the scheduler-owned AxTaskRef/ProcessData
// graph without booting global kernel runtime. This is intentionally a
// structural test of the production publication, alias-lock, action-drop,
// gate-state, and retirement primitives, not an end-to-end do_execve test.
#[test]
fn leader_and_nonleader_exec_primitives_retain_owners_through_gates() {
    struct ActionGate {
        trace: Arc<SpinNoIrq<Vec<&'static str>>>,
    }

    impl Drop for ActionGate {
        fn drop(&mut self) {
            self.trace.lock().push("action-released");
        }
    }

    fn run(nonleader: bool) {
        let leader_tid = 41;
        let executor_tid = if nonleader { 42 } else { leader_tid };
        let trace = Arc::new(SpinNoIrq::new(Vec::new()));

        let old_slot = credential_slot(1000);
        let old_slot_weak = Arc::downgrade(&old_slot);
        let executor_slot = if nonleader {
            credential_slot(2000)
        } else {
            old_slot.clone()
        };
        let old_leader_credential = old_slot.current();
        let old_leader_credential_weak = Arc::downgrade(&old_leader_credential);
        let old_executor_credential = executor_slot.current();
        let old_executor_credential_weak = Arc::downgrade(&old_executor_credential);
        let old_owner = old_leader_credential.user_ns().clone();
        let new_owner = old_executor_credential.user_ns().clone();

        let old_signal = thread_signal_manager();
        let old_signal_weak = Arc::downgrade(&old_signal);
        let new_signal = if nonleader {
            thread_signal_manager()
        } else {
            old_signal.clone()
        };
        let group = GroupLeaderIdentityBinding::try_new(old_slot.clone()).unwrap();
        group
            .bind_initial_signal(leader_tid, old_signal.clone())
            .unwrap();

        let old_state = ProcessAccessState::try_new(Dumpability::UserDumpable, old_owner).unwrap();
        let old_state_weak = Arc::downgrade(&old_state);
        let new_state = ProcessAccessState::try_new(Dumpability::NotDumpable, new_owner).unwrap();
        let expected_new_state = new_state.clone();
        let old_image = Arc::new(());
        let old_image_weak = Arc::downgrade(&old_image);
        let image = spin::RwLock::new(ProcessImageBinding {
            aspace: old_image.clone(),
            access_state: old_state.clone(),
        });

        drop((old_slot, old_leader_credential, old_executor_credential));
        drop((old_signal, old_state, old_image));

        let mut update = executor_slot.prepare();
        update.builder.ids.ruid = kuid(3000);
        update.builder.ids.euid = kuid(3000);
        update.builder.ids.suid = kuid(3000);
        update.builder.ids.fsuid = kuid(3000);
        let prepared = update.finish().unwrap();
        let visible_tid = AtomicU32::new(executor_tid);
        let new_image = Arc::new(());
        let expected_new_image = new_image.clone();
        let exec_ctl = SpinNoIrq::new(ExecControlState {
            owner: Some(executor_tid),
            ..ExecControlState::default()
        });
        let vfork_ctl = SpinNoIrq::new(VforkControlState {
            parent_tid: Some(7),
        });

        reset_commoncap_post_commit_probe();
        let publish_image = || {
            trace.lock().push("image-published");
            assert_eq!(task_alias_lock_held(), nonleader);
            assert_eq!(exec_ctl.lock().owner, Some(executor_tid));
            assert_eq!(vfork_ctl.lock().parent_tid, Some(7));
            replace_process_image_with_group_handoff(
                &image,
                &group,
                executor_slot.clone(),
                Some(GroupLeaderSignalIdentity::new(executor_tid, new_signal)),
                Some(prepared),
                None,
                ProcessImageBinding {
                    aspace: new_image,
                    access_state: new_state,
                },
                || {},
            )
        };
        let (commit, retired_image) = if nonleader {
            commit_exec_alias_publication_for_test(publish_image, || {
                assert!(task_alias_lock_held());
                visible_tid.store(leader_tid, Ordering::Release);
                trace.lock().push("alias-published");
            })
        } else {
            publish_image()
        };

        assert!(!task_alias_lock_held());
        assert_eq!(
            visible_tid.load(Ordering::Acquire),
            if nonleader { leader_tid } else { executor_tid }
        );
        let (published_cred, dumpability, _, published_image, published_state) =
            snapshot_group_credential_image(&image, &group);
        assert!(Arc::ptr_eq(&published_cred, &executor_slot.current()));
        assert_eq!(published_cred.ids().euid, kuid(3000));
        assert_eq!(dumpability, Dumpability::NotDumpable);
        assert!(Arc::ptr_eq(&published_image, &expected_new_image));
        assert!(Arc::ptr_eq(&published_state, &expected_new_state));
        assert!(old_leader_credential_weak.upgrade().is_some());
        assert!(old_executor_credential_weak.upgrade().is_some());
        assert!(old_state_weak.upgrade().is_some());
        assert!(old_image_weak.upgrade().is_some());

        let retirement = commit.complete_post_commit();
        trace.lock().push("credential-committed");
        let (count, old_uid, new_uid, _) = commoncap_post_commit_probe();
        assert_eq!(count, 1);
        assert_eq!(old_uid, if nonleader { 2000 } else { 1000 });
        assert_eq!(new_uid, 3000);
        assert!(old_leader_credential_weak.upgrade().is_some());
        assert!(old_executor_credential_weak.upgrade().is_some());
        assert!(old_state_weak.upgrade().is_some());
        assert!(old_image_weak.upgrade().is_some());
        if nonleader {
            assert!(old_signal_weak.upgrade().is_some());
        }

        let completed = release_exec_action_then_complete(
            ActionGate {
                trace: trace.clone(),
            },
            || {
                assert_eq!(exec_ctl.lock().owner, Some(executor_tid));
                assert_eq!(vfork_ctl.lock().parent_tid, Some(7));
                trace.lock().push("full-image-committed");
                (retirement, retired_image)
            },
        );
        assert_eq!(exec_ctl.lock().owner, Some(executor_tid));
        assert_eq!(vfork_ctl.lock().parent_tid, Some(7));
        assert!(old_leader_credential_weak.upgrade().is_some());
        assert!(old_executor_credential_weak.upgrade().is_some());
        assert!(old_state_weak.upgrade().is_some());
        assert!(old_image_weak.upgrade().is_some());

        assert!(release_exec_control_owner(&exec_ctl, executor_tid));
        trace.lock().push("exec-gate-released");
        assert_eq!(exec_ctl.lock().owner, None);
        assert_eq!(vfork_ctl.lock().parent_tid, Some(7));
        assert!(old_leader_credential_weak.upgrade().is_some());
        assert!(old_image_weak.upgrade().is_some());
        assert!(release_vfork_control_parent(&vfork_ctl));
        trace.lock().push("vfork-gate-released");
        assert_eq!(vfork_ctl.lock().parent_tid, None);
        assert!(old_leader_credential_weak.upgrade().is_some());
        assert!(old_image_weak.upgrade().is_some());

        drop(completed);
        reclaim_deferred_credential_owners();
        trace.lock().push("retirement-dropped");
        assert!(old_leader_credential_weak.upgrade().is_none());
        assert!(old_executor_credential_weak.upgrade().is_none());
        assert!(old_state_weak.upgrade().is_none());
        assert!(old_image_weak.upgrade().is_none());
        if nonleader {
            assert!(old_slot_weak.upgrade().is_none());
            assert!(old_signal_weak.upgrade().is_none());
        } else {
            assert!(old_slot_weak.upgrade().is_some());
            assert!(old_signal_weak.upgrade().is_some());
        }

        let expected = if nonleader {
            vec![
                "image-published",
                "alias-published",
                "credential-committed",
                "action-released",
                "full-image-committed",
                "exec-gate-released",
                "vfork-gate-released",
                "retirement-dropped",
            ]
        } else {
            vec![
                "image-published",
                "credential-committed",
                "action-released",
                "full-image-committed",
                "exec-gate-released",
                "vfork-gate-released",
                "retirement-dropped",
            ]
        };
        assert_eq!(*trace.lock(), expected);
    }

    run(false);
    run(true);
}

#[test]
fn group_leader_scheduler_snapshot_retains_last_successful_policy_through_exit_owner() {
    let group = GroupLeaderIdentityBinding::try_new(credential_slot(0)).unwrap();
    group
        .bind_initial_signal(41, thread_signal_manager())
        .unwrap();
    let owner = group.signal_owner();
    let scheduler = owner
        .lock()
        .as_ref()
        .and_then(|identity| identity.scheduler.clone())
        .expect("initial group-leader signal owner has scheduler snapshot");

    assert_eq!(*scheduler.lock(), ZombieSchedulerSnapshot::default());

    group.publish_scheduler_state(
        41,
        SchedState {
            class: SchedClass::Normal,
            nice: 19,
            rt_priority: 0,
        },
        0,
    );
    group.publish_scheduler_state(
        41,
        SchedState {
            class: SchedClass::Idle,
            nice: 19,
            rt_priority: 0,
        },
        0,
    );

    let expected = ZombieSchedulerSnapshot {
        class: SchedClass::Idle,
        nice: 19,
        rt_priority: 0,
        reset_on_fork: false,
        uclamp_min: 0,
        uclamp_max: 1024,
        uclamp_min_user_defined: false,
        uclamp_max_user_defined: false,
        uclamp_effective_min: 0,
        uclamp_effective_max: 1024,
        affinity: AxCpuMask::full(),
        identity_epoch: 0,
        version: 0,
    };
    assert_eq!(*scheduler.lock(), expected);
    assert_eq!(
        *owner
            .lock()
            .as_ref()
            .and_then(|identity| identity.scheduler.clone())
            .unwrap()
            .lock(),
        expected
    );
}

#[test]
fn group_leader_handoff_reseeds_scheduler_snapshot_in_a_new_identity_epoch() {
    let binding = GroupLeaderIdentityBinding::try_new(credential_slot(1000)).unwrap();
    binding
        .bind_initial_signal(9, thread_signal_manager())
        .unwrap();
    let owner = binding.signal_owner();

    binding.publish_scheduler_state(
        9,
        SchedState {
            class: SchedClass::Idle,
            nice: 19,
            rt_priority: 0,
        },
        100,
    );
    let handoff = binding.publish_handoff(
        credential_slot(2000),
        Some(GroupLeaderSignalIdentity::new(10, thread_signal_manager())),
        None,
        Some(scheduler_snapshot(
            SchedState {
                class: SchedClass::Fifo,
                nice: 0,
                rt_priority: 73,
            },
            3,
        )),
    );
    drop(handoff.complete_post_commit());

    let scheduler = owner
        .lock()
        .as_ref()
        .and_then(|identity| identity.scheduler.clone())
        .unwrap();
    assert_eq!(
        *scheduler.lock(),
        ZombieSchedulerSnapshot {
            class: SchedClass::Fifo,
            nice: 0,
            rt_priority: 73,
            reset_on_fork: false,
            uclamp_min: 0,
            uclamp_max: 1024,
            uclamp_min_user_defined: false,
            uclamp_max_user_defined: false,
            uclamp_effective_min: 0,
            uclamp_effective_max: UtilizationBounds::unrestricted().maximum as u16,
            affinity: AxCpuMask::full(),
            identity_epoch: 1,
            version: 3,
        }
    );

    // The retired leader's larger local version is not comparable with
    // the executor's stream and must not overwrite the new binding.
    binding.publish_scheduler_state(
        9,
        SchedState {
            class: SchedClass::Fifo,
            nice: 0,
            rt_priority: 1,
        },
        101,
    );
    binding.publish_scheduler_state(
        10,
        SchedState {
            class: SchedClass::Batch,
            nice: 4,
            rt_priority: 0,
        },
        4,
    );
    assert_eq!(
        *scheduler.lock(),
        ZombieSchedulerSnapshot {
            class: SchedClass::Batch,
            nice: 4,
            rt_priority: 0,
            reset_on_fork: false,
            uclamp_min: 0,
            uclamp_max: 1024,
            uclamp_min_user_defined: false,
            uclamp_max_user_defined: false,
            uclamp_effective_min: 0,
            uclamp_effective_max: 1024,
            affinity: AxCpuMask::full(),
            identity_epoch: 1,
            version: 4,
        }
    );
}

#[test]
fn scheduler_handoff_accepts_new_task_version_zero_after_old_version_five() {
    let old = scheduler_snapshot(
        SchedState {
            class: SchedClass::Fifo,
            nice: 0,
            rt_priority: 1,
        },
        5,
    );
    let new = scheduler_snapshot(
        SchedState {
            class: SchedClass::Normal,
            nice: -4,
            rt_priority: 0,
        },
        0,
    );
    // The token changes with identity, so version streams are never
    // compared across the old leader and a new executor.
    assert!(scheduler_publication_matches(18, 18, new, Some(new)));
    assert_ne!(old.version, new.version);
}

#[test]
fn scheduler_publication_rejects_remote_state_version_change() {
    let committed = scheduler_snapshot(
        SchedState {
            class: SchedClass::Normal,
            nice: 3,
            rt_priority: 0,
        },
        7,
    );
    let remote = scheduler_snapshot(
        SchedState {
            class: SchedClass::Batch,
            nice: 8,
            rt_priority: 0,
        },
        8,
    );
    assert!(!scheduler_publication_matches(
        4,
        4,
        committed,
        Some(remote)
    ));
}

#[test]
fn delayed_old_leader_scheduler_publication_is_rejected_by_token() {
    let commit = scheduler_snapshot(SchedState::default(), 5);
    assert!(!scheduler_publication_matches(12, 11, commit, Some(commit)));
}

#[test]
fn scheduler_commit_before_exec_cannot_admit_after_leader_handoff() {
    let binding = GroupLeaderIdentityBinding::try_new(credential_slot(1000)).unwrap();
    binding
        .bind_initial_signal(9, thread_signal_manager())
        .unwrap();
    let old_token = binding.publication_token_for(9);
    let handoff = binding.publish_handoff(
        credential_slot(2000),
        Some(GroupLeaderSignalIdentity::new(10, thread_signal_manager())),
        None,
        Some(scheduler_snapshot(SchedState::default(), 0)),
    );
    drop(handoff.complete_post_commit());

    // The commit completed before exec but publication admission is after
    // exec: the retired executor is no longer the durable leader.
    assert_eq!(old_token, Some(0));
    assert_eq!(binding.publication_token_for(9), None);
}

#[test]
fn scheduler_commit_after_exec_seed_admits_under_new_leader_token() {
    let binding = GroupLeaderIdentityBinding::try_new(credential_slot(1000)).unwrap();
    binding
        .bind_initial_signal(9, thread_signal_manager())
        .unwrap();
    let handoff = binding.publish_handoff(
        credential_slot(2000),
        Some(GroupLeaderSignalIdentity::new(10, thread_signal_manager())),
        None,
        Some(scheduler_snapshot(SchedState::default(), 0)),
    );
    drop(handoff.complete_post_commit());

    let commit = scheduler_snapshot(
        SchedState {
            class: SchedClass::Batch,
            nice: 6,
            rt_priority: 0,
        },
        1,
    );
    let token = binding.publication_token_for(10);
    assert_eq!(token, Some(1));
    assert!(scheduler_publication_matches(
        token.unwrap(),
        token.unwrap(),
        commit,
        Some(commit)
    ));
}

#[test]
fn binding_switched_before_visible_tid_alias_admits_new_executor_only() {
    let binding = GroupLeaderIdentityBinding::try_new(credential_slot(1000)).unwrap();
    binding
        .bind_initial_signal(9, thread_signal_manager())
        .unwrap();
    let handoff = binding.publish_handoff(
        credential_slot(2000),
        Some(GroupLeaderSignalIdentity::new(10, thread_signal_manager())),
        None,
        Some(scheduler_snapshot(SchedState::default(), 0)),
    );
    drop(handoff.complete_post_commit());

    // This is the window before exec publishes the executor's visible-TID
    // alias.  Admission follows the installed kernel-TID endpoint, not
    // the old/new user-visible TID value.
    assert_eq!(binding.publication_token_for(9), None);
    assert_eq!(binding.publication_token_for(10), Some(1));
}

#[test]
fn user_namespace_admission_has_a_reusable_hard_ceiling() {
    let counter = AtomicUsize::new(0);
    assert!(try_increment_bounded(&counter, 2));
    assert!(try_increment_bounded(&counter, 2));
    assert!(!try_increment_bounded(&counter, 2));
    assert_eq!(counter.fetch_sub(1, Ordering::Release), 2);
    assert!(try_increment_bounded(&counter, 2));
    assert_eq!(counter.load(Ordering::Acquire), 2);
}

#[test]
fn late_group_exit_gate_covers_core_to_task_table_window() {
    assert!(!group_exit_handoff_requires_kill(false, false));
    assert!(group_exit_handoff_requires_kill(true, false));
    assert!(group_exit_handoff_requires_kill(false, true));
    assert!(group_exit_handoff_requires_kill(true, true));
}

#[test]
fn group_leader_binding_keeps_the_single_slot_alive() {
    let slot = credential_slot(1000);
    let weak = Arc::downgrade(&slot);
    let binding = GroupLeaderIdentityBinding::try_new(slot.clone()).unwrap();
    drop(slot);

    assert_eq!(binding.current_cred().ids().ruid, kuid(1000));
    assert!(weak.upgrade().is_some());
    drop(binding);
    assert!(weak.upgrade().is_none());
}

#[test]
fn group_leader_binding_handoffs_private_signal_identity_with_credential() {
    let old_slot = credential_slot(1000);
    let new_slot = credential_slot(2000);
    let old_signal = thread_signal_manager();
    let new_signal = thread_signal_manager();
    let old_signal_weak = Arc::downgrade(&old_signal);
    let binding = GroupLeaderIdentityBinding::try_new(old_slot).unwrap();

    binding.bind_initial_signal(9, old_signal.clone()).unwrap();
    assert!(binding.bind_initial_signal(10, new_signal.clone()).is_err());
    let (credential, signal) = binding.current_cred_and_signal().unwrap();
    assert_eq!(credential.ids().ruid, kuid(1000));
    assert!(Arc::ptr_eq(&signal, &old_signal));
    drop((credential, signal, old_signal));

    let commit = binding.publish_handoff(
        new_slot,
        Some(GroupLeaderSignalIdentity::new(10, new_signal.clone())),
        None,
        None,
    );
    let (credential, signal) = binding.current_cred_and_signal().unwrap();
    assert_eq!(credential.ids().ruid, kuid(2000));
    assert!(Arc::ptr_eq(&signal, &new_signal));
    assert!(old_signal_weak.upgrade().is_some());
    drop((credential, signal));

    let retirement = commit.complete_post_commit();
    assert!(old_signal_weak.upgrade().is_some());
    drop(retirement);
    assert!(old_signal_weak.upgrade().is_none());
}

#[test]
fn group_leader_identity_snapshot_reseeds_on_same_owner_exec_handoff() {
    let slot = credential_slot(1000);
    let signal = thread_signal_manager();
    let binding = GroupLeaderIdentityBinding::try_new(slot.clone()).unwrap();
    binding.bind_initial_signal(9, signal.clone()).unwrap();

    let before = binding.identity_snapshot().unwrap();
    assert!(binding.identity_snapshot_matches(&before));

    // Leader exec retains both the task credential slot and its private
    // endpoint.  The identity token must still invalidate work which was
    // authorized before the image handoff.
    let mut update = slot.prepare();
    update.builder.ids.ruid = kuid(2000);
    update.builder.ids.euid = kuid(2000);
    update.builder.ids.suid = kuid(2000);
    update.builder.ids.fsuid = kuid(2000);
    let prepared = update.finish().unwrap();
    drop(
        binding
            .publish_handoff(
                slot.clone(),
                Some(GroupLeaderSignalIdentity::new(9, signal)),
                Some(prepared),
                None,
            )
            .complete_post_commit(),
    );

    let after = binding.identity_snapshot().unwrap();
    assert_ne!(before.token(), after.token());
    assert!(Arc::ptr_eq(before.signal(), after.signal()));
    assert!(!binding.identity_snapshot_matches(&before));
    assert!(binding.identity_snapshot_matches(&after));
    assert_eq!(after.credential().ids().ruid, kuid(2000));
}

#[test]
fn group_leader_successful_reap_releases_private_and_shared_signal_charges_once() {
    let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let process = Arc::new(ProcessSignalManager::new(actions, 0));
    let leader = registered_thread_signal_manager(process.clone(), 9);
    let per_user = SignalQueueAccount::try_new(4).unwrap();
    let global = SignalQueueAccount::try_new(4).unwrap();

    enqueue_accounted_signal(&leader, Signo::SIGRTMIN, &per_user, &global);
    enqueue_accounted_process_signal(&process, Signo::SIGRTMIN, &per_user, &global);
    assert_eq!((per_user.queued(), global.queued()), (2, 2));

    // Final exit preserves both queues through zombie lifetime.
    leader.retire_registration(9, true);
    process.retain_pending_only();
    assert_eq!((per_user.queued(), global.queued()), (2, 2));

    let owner = Arc::new(SpinNoIrq::new(Some(GroupLeaderSignalIdentity::new(
        9,
        leader.clone(),
    ))));
    let retained_snapshot_owner = owner.clone();
    assert!(retire_group_leader_signal_owner(&owner));
    assert_eq!((per_user.queued(), global.queued()), (0, 0));
    assert!(retained_snapshot_owner.lock().is_none());
    assert!(!retire_group_leader_signal_owner(&retained_snapshot_owner));
    assert_eq!((per_user.queued(), global.queued()), (0, 0));
}

#[test]
fn group_leader_exec_replacement_retires_old_but_preserves_same_endpoint() {
    let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let process = Arc::new(ProcessSignalManager::new(actions, 0));
    let old_signal = registered_thread_signal_manager(process.clone(), 9);
    let new_signal = registered_thread_signal_manager(process, 10);
    let old_slot = credential_slot(1000);
    let new_slot = credential_slot(2000);
    let binding = GroupLeaderIdentityBinding::try_new(old_slot).unwrap();
    binding.bind_initial_signal(9, old_signal.clone()).unwrap();
    let old_user = SignalQueueAccount::try_new(2).unwrap();
    let old_global = SignalQueueAccount::try_new(2).unwrap();
    enqueue_accounted_signal(&old_signal, Signo::SIGRTMIN, &old_user, &old_global);

    let replacement = binding.publish_handoff(
        new_slot.clone(),
        Some(GroupLeaderSignalIdentity::new(10, new_signal.clone())),
        None,
        None,
    );
    assert_eq!(old_user.queued(), 1);
    drop(replacement.complete_post_commit());
    assert_eq!((old_user.queued(), old_global.queued()), (0, 0));

    let new_user = SignalQueueAccount::try_new(2).unwrap();
    let new_global = SignalQueueAccount::try_new(2).unwrap();
    enqueue_accounted_signal(&new_signal, Signo::SIGRTMIN, &new_user, &new_global);
    let same_endpoint = binding.publish_handoff(
        new_slot,
        Some(GroupLeaderSignalIdentity::new(10, new_signal.clone())),
        None,
        None,
    );
    drop(same_endpoint.complete_post_commit());
    assert_eq!((new_user.queued(), new_global.queued()), (1, 1));
    new_signal.retire_registration(10, false);
    assert_eq!((new_user.queued(), new_global.queued()), (0, 0));
}

#[test]
fn group_leader_repeated_exec_handoffs_retire_each_registration_tid() {
    let actions = SharedSignalActions::try_new(SignalActions::default()).unwrap();
    let process = Arc::new(ProcessSignalManager::new(actions, 0));
    let first = registered_thread_signal_manager(process.clone(), 9);
    let second = registered_thread_signal_manager(process.clone(), 10);
    let third = registered_thread_signal_manager(process, 11);
    let binding = GroupLeaderIdentityBinding::try_new(credential_slot(1000)).unwrap();
    binding.bind_initial_signal(9, first.clone()).unwrap();

    drop(
        binding
            .publish_handoff(
                credential_slot(2000),
                Some(GroupLeaderSignalIdentity::new(10, second.clone())),
                None,
                None,
            )
            .complete_post_commit(),
    );
    drop(
        binding
            .publish_handoff(
                credential_slot(3000),
                Some(GroupLeaderSignalIdentity::new(11, third.clone())),
                None,
                None,
            )
            .complete_post_commit(),
    );

    assert!(!first.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 1, 0)));
    assert!(!second.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 1, 0)));
    assert!(third.send_unqueued_signal(SignalInfo::new_user(Signo::SIGTERM, 0, 1, 0)));
    third.retire_registration(11, false);
}

#[test]
fn group_leader_handoff_never_exposes_the_unprepared_slot() {
    const READS: usize = 20_000;

    let old = credential_slot(1000);
    let new = credential_slot(2000);
    let binding = Arc::new(GroupLeaderIdentityBinding::try_new(old).unwrap());
    let start = Arc::new(Barrier::new(2));
    let reader = {
        let binding = binding.clone();
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            for _ in 0..READS {
                let uid = binding.current_cred().ids().ruid;
                assert!(
                    uid == kuid(1000) || uid == kuid(3000),
                    "mixed handoff uid {uid:?}"
                );
            }
        })
    };

    let mut update = new.prepare();
    update.builder.ids.ruid = kuid(3000);
    update.builder.ids.euid = kuid(3000);
    update.builder.ids.suid = kuid(3000);
    update.builder.ids.fsuid = kuid(3000);
    let prepared = update.finish().unwrap();
    start.wait();
    let commit = binding.publish_handoff(new.clone(), None, Some(prepared), None);
    assert_eq!(binding.current_cred().ids().ruid, kuid(3000));
    let retirement = commit.complete_post_commit();
    drop(retirement);
    reader.join().unwrap();
}

#[test]
fn signal_accounts_are_keyed_by_user_namespace_and_real_uid() {
    let first_ns = UserNamespace::try_new_root().unwrap();
    let second_ns = UserNamespace::try_new_root().unwrap();

    let (first, first_global) = first_ns.try_signal_queue_accounts(kuid(1000)).unwrap();
    let (same, same_global) = first_ns.try_signal_queue_accounts(kuid(1000)).unwrap();
    let (other_uid, _) = first_ns.try_signal_queue_accounts(kuid(1001)).unwrap();
    let (other_ns, other_global) = second_ns.try_signal_queue_accounts(kuid(1000)).unwrap();

    assert!(Arc::ptr_eq(&first, &same));
    assert!(Arc::ptr_eq(&first_global, &same_global));
    assert!(!Arc::ptr_eq(&first, &other_uid));
    assert!(!Arc::ptr_eq(&first, &other_ns));
    assert!(!Arc::ptr_eq(&first_global, &other_global));
}

#[test]
fn descendant_user_namespaces_share_the_root_global_account_only() {
    let root = UserNamespace::try_new_root().unwrap();
    let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
    child
        .publish_uid_map(
            child
                .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
                .unwrap(),
        )
        .unwrap();
    child
        .publish_gid_map(
            child
                .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
                .unwrap(),
            false,
        )
        .unwrap();
    let grandchild = child.try_fork(kuid(1000), kgid(1000), false).unwrap();

    let (root_user, root_global) = root.try_signal_queue_accounts(kuid(1000)).unwrap();
    let (child_user, child_global) = child.try_signal_queue_accounts(kuid(1000)).unwrap();
    let (grandchild_user, grandchild_global) =
        grandchild.try_signal_queue_accounts(kuid(1000)).unwrap();

    assert!(!Arc::ptr_eq(&root_user, &child_user));
    assert!(!Arc::ptr_eq(&child_user, &grandchild_user));
    assert!(Arc::ptr_eq(&root_global, &child_global));
    assert!(Arc::ptr_eq(&root_global, &grandchild_global));
}

#[test]
fn user_namespace_maps_publish_once_and_setgroups_deny_is_irreversible() {
    let root = UserNamespace::try_new_root().unwrap();
    let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
    assert!(child.uid_map().is_empty());
    assert!(child.gid_map().is_empty());

    let uid_map = child
        .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    child.publish_uid_map(uid_map).unwrap();
    assert_eq!(child.kernel_uid_to_user(kuid(1000)).unwrap().into_raw(), 0);
    assert_eq!(
        child.publish_uid_map(
            child
                .try_build_uid_map(vec![IdMapInputExtent::new(1, 1001, 1)])
                .unwrap()
        ),
        Err(AxError::OperationNotPermitted)
    );

    let gid_map = child
        .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    assert_eq!(
        child.publish_gid_map(gid_map.clone(), true),
        Err(AxError::OperationNotPermitted)
    );
    child.update_setgroups_policy(false).unwrap();
    assert!(!child.setgroups_allowed());
    assert_eq!(
        child.update_setgroups_policy(true),
        Err(AxError::OperationNotPermitted)
    );
    child.publish_gid_map(gid_map, true).unwrap();
    assert!(!child.may_setgroups());
    assert_eq!(
        child.update_setgroups_policy(false),
        Err(AxError::OperationNotPermitted)
    );
}

#[test]
fn concurrent_uid_map_publish_has_exactly_one_winner() {
    let root = UserNamespace::try_new_root().unwrap();
    let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
    let first = child
        .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    let second = child
        .try_build_uid_map(vec![IdMapInputExtent::new(1, 2000, 1)])
        .unwrap();
    let start = Arc::new(Barrier::new(2));

    let first_publisher = {
        let child = child.clone();
        let map = first.clone();
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            child.publish_uid_map(map)
        })
    };
    let second_publisher = {
        let child = child.clone();
        let map = second.clone();
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            child.publish_uid_map(map)
        })
    };

    let mut successes = 0;
    let mut duplicate_rejections = 0;
    for result in [
        first_publisher.join().unwrap(),
        second_publisher.join().unwrap(),
    ] {
        match result {
            Ok(()) => successes += 1,
            Err(AxError::OperationNotPermitted) => duplicate_rejections += 1,
            other => panic!("unexpected UID map publication result: {other:?}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(duplicate_rejections, 1);

    let published = child.uid_map();
    assert!(Arc::ptr_eq(&published, &first) || Arc::ptr_eq(&published, &second));
}

#[test]
fn uid_map_reader_race_observes_only_empty_or_complete_immutable_snapshots() {
    const READS: usize = 20_000;

    fn assert_complete(map: &IdMap) {
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.kernel_uid_to_user(kuid(1000)).map(|uid| uid.into_raw()),
            Some(0)
        );
        assert_eq!(
            map.kernel_uid_to_user(kuid(1001)).map(|uid| uid.into_raw()),
            Some(1)
        );
        assert_eq!(
            map.kernel_uid_to_user(kuid(2000)).map(|uid| uid.into_raw()),
            Some(100)
        );
        assert_eq!(
            map.kernel_uid_to_user(kuid(2001)).map(|uid| uid.into_raw()),
            Some(101)
        );
        assert_eq!(map.kernel_uid_to_user(kuid(1500)), None);
    }

    let root = UserNamespace::try_new_root().unwrap();
    let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
    let replacement = child
        .try_build_uid_map(vec![
            IdMapInputExtent::new(0, 1000, 2),
            IdMapInputExtent::new(100, 2000, 2),
        ])
        .unwrap();
    let empty_snapshot = child.uid_map();
    assert!(empty_snapshot.is_empty());

    let start = Arc::new(Barrier::new(2));
    let publisher = {
        let child = child.clone();
        let replacement = replacement.clone();
        let start = start.clone();
        thread::spawn(move || {
            start.wait();
            child.publish_uid_map(replacement)
        })
    };

    start.wait();
    for index in 0..READS {
        let snapshot = child.uid_map();
        if !snapshot.is_empty() {
            assert_complete(&snapshot);
        }
        if index % 64 == 0 {
            thread::yield_now();
        }
    }
    publisher.join().unwrap().unwrap();

    assert!(empty_snapshot.is_empty());
    let published = child.uid_map();
    assert!(Arc::ptr_eq(&published, &replacement));
    assert_complete(&published);
}

#[test]
fn setgroups_deny_race_preserves_gid_gate_and_failed_publish_is_retryable() {
    const RACES: usize = 64;
    const SAMPLES_PER_RACE: usize = 128;

    // Exercise the publication-first result deterministically: failure
    // keeps the slot empty, and the exact prebuilt map can be retried after
    // the irreversible deny transition.
    let retry_root = UserNamespace::try_new_root().unwrap();
    let retry_child = retry_root.try_fork(kuid(1000), kgid(1000), false).unwrap();
    let retry_map = retry_child
        .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
        .unwrap();
    assert_eq!(
        retry_child.publish_gid_map(retry_map.clone(), true),
        Err(AxError::OperationNotPermitted)
    );
    assert!(!retry_child.gid_map_written());
    retry_child.update_setgroups_policy(false).unwrap();
    retry_child
        .publish_gid_map(retry_map.clone(), true)
        .unwrap();
    assert!(Arc::ptr_eq(&retry_child.gid_map(), &retry_map));

    for _ in 0..RACES {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        let map = child
            .try_build_gid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
            .unwrap();
        let start = Arc::new(Barrier::new(3));

        let deny = {
            let child = child.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                child.update_setgroups_policy(false)
            })
        };
        let publish = {
            let child = child.clone();
            let map = map.clone();
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                child.publish_gid_map(map, true)
            })
        };

        start.wait();
        for sample in 0..SAMPLES_PER_RACE {
            let state = child.map_state.lock();
            assert!(
                !state.setgroups_allowed() || !state.gid_map_written(),
                "require-denied GID map became visible while setgroups was allowed"
            );
            drop(state);
            if sample % 16 == 0 {
                thread::yield_now();
            }
        }

        deny.join().unwrap().unwrap();
        let publish_result = publish.join().unwrap();
        assert!(!child.setgroups_allowed());
        match publish_result {
            Ok(()) => assert!(child.gid_map_written()),
            Err(AxError::OperationNotPermitted) => {
                assert!(!child.gid_map_written());
                child.publish_gid_map(map.clone(), true).unwrap();
            }
            other => panic!("unexpected GID map publication result: {other:?}"),
        }
        let state = child.map_state.lock();
        assert!(!state.setgroups_allowed());
        assert!(state.gid_map_written());
        assert!(!state.may_setgroups());
        drop(state);
        assert!(Arc::ptr_eq(&child.gid_map(), &map));
    }
}

#[test]
fn nested_user_namespace_owner_must_be_mapped_in_parent() {
    let root = UserNamespace::try_new_root().unwrap();
    let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
    assert!(matches!(
        child.try_fork(kuid(1000), kgid(1000), false),
        Err(AxError::OperationNotPermitted)
    ));
}

#[test]
fn namespace_owner_objects_retain_explicit_snapshot_and_forked_state() {
    let _context = crate::test_support::scheduler_test_context();
    let root = UserNamespace::try_new_root().unwrap();
    let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
    let child_weak = Arc::downgrade(&child);

    let cgroup_root = CgroupNamespace::try_new_root(root.clone()).unwrap();
    let cgroup_child = CgroupNamespace::try_fork(
        &cgroup_root,
        child.clone(),
        crate::pseudofs::cgroup::root_namespace_roots().unwrap(),
    )
    .unwrap();
    assert!(Arc::ptr_eq(cgroup_root.owner_user_ns(), &root));
    assert!(Arc::ptr_eq(cgroup_child.owner_user_ns(), &child));

    let pid_root = PidNamespace::try_new_root(root.clone()).unwrap();
    let pid_child = pid_root.try_fork(42, child.clone()).unwrap();
    assert!(Arc::ptr_eq(pid_root.owner_user_ns(), &root));
    assert!(Arc::ptr_eq(pid_child.owner_user_ns(), &child));

    let uts_root = UtsNamespace::try_new_root(root.clone()).unwrap();
    uts_root.set_nodename(b"owner-snapshot").unwrap();
    let uts_child = uts_root.try_fork(child.clone()).unwrap();
    assert!(Arc::ptr_eq(uts_child.owner_user_ns(), &child));
    assert_eq!(uts_child.nodename().unwrap(), b"owner-snapshot");

    let time_root = TimeNamespace::try_new_root(root.clone()).unwrap();
    time_root.set_monotonic_offset(7, 11);
    time_root.set_boottime_offset(-3, 19);
    let time_child = time_root.try_fork(child.clone()).unwrap();
    assert!(Arc::ptr_eq(time_child.owner_user_ns(), &child));
    assert_eq!(time_child.render_offsets(), time_root.render_offsets());

    let network_child = NetworkNamespace::try_new_loopback_only(child.clone()).unwrap();
    assert!(Arc::ptr_eq(network_child.owner_user_ns(), &child));

    drop(child);
    assert!(child_weak.upgrade().is_some());
    drop((
        cgroup_child,
        pid_child,
        uts_child,
        time_child,
        network_child,
    ));
    assert!(child_weak.upgrade().is_none());
}

#[test]
fn concurrent_registry_admission_publishes_one_live_winner() {
    const THREADS: usize = 16;

    let namespace = UserNamespace::try_new_root().unwrap();
    let start = Arc::new(Barrier::new(THREADS));
    let hold = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let namespace = namespace.clone();
            let start = start.clone();
            let hold = hold.clone();
            thread::spawn(move || {
                start.wait();
                let account = namespace.try_signal_queue_accounts(kuid(1000)).unwrap().0;
                // Keep every returned strong reference alive until all
                // racing lookups have completed.
                hold.wait();
                account
            })
        })
        .collect();
    let accounts: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    assert!(
        accounts[1..]
            .iter()
            .all(|account| Arc::ptr_eq(&accounts[0], account))
    );
}

#[test]
fn implementation_signal_queue_ceilings_are_bounded() {
    assert_eq!(SIGNAL_QUEUE_PER_USER_HARD_LIMIT, 4_096);
    assert_eq!(SIGNAL_QUEUE_GLOBAL_HARD_LIMIT, 16_384);
}

#[test]
fn pid_namespace_bindings_cover_ancestors_and_survive_until_reap() {
    let user_ns = UserNamespace::try_new_root().unwrap();
    let root = PidNamespace::try_new_root(user_ns.clone()).unwrap();
    let root_init = root.reserve_process(10).unwrap();
    root_init.commit();
    assert_eq!(root.visible_pid(10), 1);
    assert_eq!(root.resolve_visible_pid(1), Some(10));
    assert_eq!(root.resolve_visible_pid(0), None);
    assert_eq!(root.resolve_visible_pid(99), None);

    // A CLONE_NEWPID child is PID 1 locally, while the parent namespace
    // receives its independent next local PID binding.
    let child = root.try_fork(20, user_ns).unwrap();
    let child_init = child.reserve_process(20).unwrap();
    child_init.commit();
    assert_eq!(child.visible_pid(20), 1);
    assert_eq!(child.resolve_visible_pid(1), Some(20));
    assert_eq!(root.visible_pid_for(&child, 20), Some(2));
    assert_eq!(root.resolve_visible_pid(2), Some(20));

    let unpublished = child.reserve_process(21).unwrap();
    assert_eq!(child.visible_pid(21), 2);
    drop(unpublished);
    assert!(!child.pids.lock().by_global.contains_key(&21));
    assert!(!root.pids.lock().by_global.contains_key(&21));

    let live = child.reserve_process(22).unwrap();
    live.commit();
    // Releasing an unpublished reservation does not rewind the allocator:
    // like Linux's cyclic PID allocator, the next admission advances past
    // the discarded candidate.
    assert_eq!(child.visible_pid(22), 3);
    assert_eq!(child.resolve_visible_pid(3), Some(22));
    assert_eq!(root.visible_pid_for(&child, 22), Some(4));
    child.release_reaped_process(22);
    assert!(!child.pids.lock().by_global.contains_key(&22));
    assert!(!root.pids.lock().by_global.contains_key(&22));
    assert_eq!(child.resolve_visible_pid(3), None);

    // Allocation is cyclic rather than LIFO: a released PID is reusable,
    // but Linux need not return it for the immediately following fork.
    let next = child.reserve_process(23).unwrap();
    next.commit();
    assert_eq!(child.visible_pid(23), 4);
    assert_eq!(root.visible_pid_for(&child, 23), Some(5));
}

#[test]
fn pid_namespace_shutdown_drains_admitted_clones_and_closes_descendants() {
    let owner = UserNamespace::try_new_root().unwrap();
    let actor = Cred::try_root(owner.clone()).unwrap();
    let root = PidNamespace::try_new_root(owner.clone()).unwrap();
    root.reserve_process(10).unwrap().commit();
    let child = root.try_fork(20, owner.clone()).unwrap();
    child.reserve_process(20).unwrap().commit();
    let nested = child.try_fork(30, owner).unwrap();
    nested.reserve_process(30).unwrap().commit();
    let admitted = nested.reserve_process(31).unwrap();
    let cancelled = child.reserve_process(21).unwrap();

    child.disable_allocation();
    assert!(child.has_pending_publications());
    admitted.commit();
    assert!(child.has_pending_publications());
    drop(cancelled);
    assert!(!child.has_pending_publications());
    assert!(!root.has_pending_publications());
    assert!(!nested.has_pending_publications());
    assert_eq!(nested.visible_pid_checked(31), Some(2));
    assert_eq!(child.visible_pid_checked(21), None);

    for ids in [&[][..], &[7, 8, 9][..]] {
        assert!(matches!(
            nested.reserve_process_with_ids(40, ids, &actor),
            Err(AxError::NoMemory)
        ));
        assert_eq!(nested.visible_pid_checked(40), None);
    }
    assert!(matches!(nested.reserve_process(41), Err(AxError::NoMemory)));
    assert!(!nested.has_pending_publications());
    // Closing a child never disables unrelated outer-namespace forks.
    root.reserve_process(50).unwrap().commit();
}

#[test]
fn exact_pid_slots_reject_invalid_and_colliding_values() {
    let mut pids = PidNamespacePids::try_new(None).unwrap();
    assert_eq!(pids.try_reserve_exact(10, 42), Ok(true));
    assert_eq!(pids.by_global.get(&10), Some(&42));
    assert_eq!(pids.by_local.get(&42), Some(&10));
    // The preinstalled PID 1 of a new namespace is retried by the common
    // transaction and must remain part of that transaction without a
    // duplicate allocation.
    assert_eq!(pids.try_reserve_exact(10, 42), Ok(false));
    assert_eq!(pids.try_reserve_exact(11, 42), Err(AxError::AlreadyExists));
    assert_eq!(pids.try_reserve_exact(12, 0), Err(AxError::InvalidInput));
    assert_eq!(
        pids.try_reserve_exact(12, PID_MAX_LIMIT),
        Err(AxError::InvalidInput)
    );
}

#[test]
fn clone3_set_tid_reserves_each_namespace_and_rolls_back_on_outer_collision() {
    let owner = UserNamespace::try_new_root().unwrap();
    let actor = Cred::try_root(owner.clone()).unwrap();
    let root = PidNamespace::try_new_root(owner.clone()).unwrap();
    root.reserve_process(100).unwrap().commit();
    let child = root.try_fork(101, owner).unwrap();

    child
        .reserve_process_with_ids(102, &[7, 42], &actor)
        .unwrap()
        .commit();
    assert_eq!(child.visible_pid(102), 7);
    assert_eq!(root.visible_pid_for(&child, 102), Some(42));

    // The inner slot is acquired before the outer collision; dropping
    // the returned error must nevertheless leave no partial inner slot.
    assert!(matches!(
        child.reserve_process_with_ids(103, &[8, 42], &actor),
        Err(AxError::AlreadyExists)
    ));
    assert!(!child.pids.lock().by_global.contains_key(&103));
    assert!(matches!(
        child.reserve_process_with_ids(104, &[2, 3, 4], &actor),
        Err(AxError::InvalidInput)
    ));
}

#[test]
fn pid_namespace_with_released_reaper_remains_closed() {
    let owner = UserNamespace::try_new_root().unwrap();
    let actor = Cred::try_root(owner.clone()).unwrap();
    let domain = super::ProcessDomain::try_new().unwrap();
    let namespace =
        PidNamespace::try_new_root_with_reaper_scope(owner, domain.root_reaper_scope()).unwrap();
    namespace.reserve_process(100).unwrap().commit();
    let init = domain.try_new_init(100, None).unwrap();
    let weak_init = Arc::downgrade(&init);
    drop(init);
    drop(domain);
    assert!(weak_init.upgrade().is_none());
    assert!(!namespace.child_reaper_allows_new_processes());
    assert!(matches!(
        namespace.reserve_process(101),
        Err(AxError::NoMemory)
    ));
    assert!(matches!(
        namespace.reserve_process_with_ids(101, &[2], &actor),
        Err(AxError::NoMemory)
    ));
}

#[test]
fn clone3_set_tid_rejects_dead_ancestor_and_rolls_back_inner_slot() {
    let owner = UserNamespace::try_new_root().unwrap();
    let actor = Cred::try_root(owner.clone()).unwrap();
    let domain = super::ProcessDomain::try_new().unwrap();
    let root =
        PidNamespace::try_new_root_with_reaper_scope(owner.clone(), domain.root_reaper_scope())
            .unwrap();
    root.reserve_process(100).unwrap().commit();
    let init = domain.try_new_init(100, None).unwrap();
    domain.prepare_thread(&init, 100).unwrap().commit().unwrap();
    let child = root.try_fork(101, owner).unwrap();
    child.reserve_process(101).unwrap().commit();
    assert!(root.child_reaper_allows_new_processes());
    assert_eq!(
        init.exit_thread(100, 0),
        tk_linux_process_adapter::ThreadExitOutcome::FinalThread
    );
    assert!(!root.child_reaper_allows_new_processes());

    // Cover explicit and automatically allocated ancestor IDs. Both
    // paths acquire an inner slot before visiting the dead ancestor.
    for requested in [&[7, 42][..], &[7][..], &[][..]] {
        assert!(matches!(
            child.reserve_process_with_ids(102, requested, &actor),
            Err(AxError::NoMemory)
        ));
        assert_eq!(child.visible_pid_checked(102), None);
        assert_eq!(root.visible_pid_checked(102), None);
    }
}

#[test]
fn namespace_identity_allocator_never_wraps_or_reuses() {
    let counter = AtomicU64::new(u64::MAX - 1);
    assert_eq!(try_allocate_namespace_id(&counter), Ok(u64::MAX - 1));
    assert_eq!(
        try_allocate_namespace_id(&counter),
        Err(axerrno::LinuxError::ENOSPC.into())
    );
    assert_eq!(
        try_allocate_namespace_id(&counter),
        Err(axerrno::LinuxError::ENOSPC.into())
    );
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn scheduler_snapshot_versions_order_across_wrap() {
    assert!(super::scheduler_version_is_newer_or_equal(0, u64::MAX));
    assert!(super::scheduler_version_is_newer_or_equal(1, u64::MAX));
    assert!(!super::scheduler_version_is_newer_or_equal(u64::MAX, 1));
}
