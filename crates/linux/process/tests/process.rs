use std::sync::Arc;

use thekernel_linux_process::{ExitOutcome, ProcessDomain, ProcessError, ThreadExitOutcome};

mod common;
use common::{Zombie, child, domain, exit_and_reap, init, zombie};

#[test]
fn domains_isolate_pid_identity_and_registry_queries() {
    let left = domain();
    let right = domain();
    let left_init = init(&left);
    let right_init = init(&right);

    assert_eq!(left_init.pid(), right_init.pid());
    assert!(!Arc::ptr_eq(&left_init, &right_init));
    assert!(Arc::ptr_eq(&left.registry().get(1).unwrap(), &left_init));
    assert!(Arc::ptr_eq(&right.registry().get(1).unwrap(), &right_init));
    assert_eq!(
        left_init.try_children(right.registry()).err(),
        Some(ProcessError::WrongDomain)
    );
    assert_eq!(
        left.prepare_fork(&right_init, 2, None).err(),
        Some(ProcessError::WrongDomain)
    );
}

#[test]
fn process_admission_is_invisible_until_commit_and_rolls_back() {
    let domain = ProcessDomain::<Zombie>::try_with_membership_limit(2).unwrap();
    let init = init(&domain);

    let admission = domain.prepare_fork(&init, 2, None).unwrap();
    let unpublished = admission.process().clone();
    assert!(domain.registry().get(2).is_none());
    assert!(init.try_children(domain.registry()).unwrap().is_empty());
    assert_eq!(domain.registry().membership_count(), 2);
    assert_eq!(
        domain.prepare_fork(&init, 3, None).err(),
        Some(ProcessError::Capacity)
    );
    drop(admission);
    assert_eq!(domain.registry().membership_count(), 1);
    assert_eq!(
        domain.exit(&unpublished, zombie(2), drop),
        Err(ProcessError::NotPublished)
    );

    let committed = child(&domain, &init, 2);
    assert!(Arc::ptr_eq(&domain.registry().get(2).unwrap(), &committed));
    assert_eq!(init.try_children(domain.registry()).unwrap().len(), 1);
    exit_and_reap(&domain, &committed);
}

#[test]
fn typed_zombie_payload_is_once_only_and_survives_runtime_exit() {
    let domain = domain();
    let init = init(&domain);
    let child = child(&domain, &init, 2);
    let payload = Arc::new(Zombie {
        wait_status: 9,
        uid: 1000,
        user_ns_cookie: 0xabc,
    });

    assert_eq!(
        domain.exit(&child, payload.clone(), drop),
        Ok(ExitOutcome::BecameZombie)
    );
    assert_eq!(
        domain.exit(&child, zombie(99), drop),
        Ok(ExitOutcome::AlreadyZombie)
    );
    assert_eq!(
        domain.prepare_fork(&child, 3, None).err(),
        Some(ProcessError::NotLive)
    );
    let retained = child.zombie_payload().unwrap();
    assert!(Arc::ptr_eq(&retained, &payload));
    assert!(domain.reap(&child).unwrap());
    assert!(!domain.reap(&child).unwrap());
}

#[test]
fn nearest_live_subreaper_inherits_children_and_zombie_notification() {
    let domain = domain();
    let init = init(&domain);
    let subreaper = child(&domain, &init, 2);
    domain
        .prepare_thread(&subreaper, 2)
        .unwrap()
        .commit()
        .unwrap();
    subreaper.set_child_subreaper(true);
    let parent = child(&domain, &subreaper, 3);
    let child = child(&domain, &parent, 4);

    assert_eq!(
        domain.exit(&child, zombie(4), drop),
        Ok(ExitOutcome::BecameZombie)
    );
    let mut inherited = Vec::new();
    assert_eq!(
        domain.exit(&parent, zombie(3), |process| inherited.push(process.pid())),
        Ok(ExitOutcome::BecameZombie)
    );
    assert_eq!(inherited, [4]);
    assert!(Arc::ptr_eq(&child.parent().unwrap(), &subreaper));

    assert!(domain.reap(&child).unwrap());
    assert!(domain.reap(&parent).unwrap());
    assert!(subreaper.remove_thread(2));
    exit_and_reap(&domain, &subreaper);
}

#[test]
fn reparent_handoff_reports_the_core_selected_child_to_reaper_mapping() {
    let domain = domain();
    let init = init(&domain);
    let subreaper = child(&domain, &init, 2);
    domain
        .prepare_thread(&subreaper, 2)
        .unwrap()
        .commit()
        .unwrap();
    subreaper.set_child_subreaper(true);
    let parent = child(&domain, &subreaper, 3);
    let children: Vec<_> = (4..12).map(|pid| child(&domain, &parent, pid)).collect();

    let mut mappings = Vec::new();
    let exit = domain.prepare_exit(&parent).unwrap();
    let committed = exit.commit_with_reparent_handoff(zombie(3), drop, |batch| {
        // Deliberately allocate and re-enter the registry from the
        // callback. Both are permitted because every core spin guard was
        // released before the handoff.
        let visible = domain.registry().try_processes().unwrap();
        assert!(!visible.is_empty());
        for moved in batch.reparented() {
            let authoritative_parent = moved.child().parent().unwrap();
            assert!(Arc::ptr_eq(&authoritative_parent, batch.reaper()));
            mappings.push((moved.child().pid(), batch.reaper().pid()));
        }
    });

    assert_eq!(committed.outcome(), ExitOutcome::BecameZombie);
    assert_eq!(mappings.len(), children.len());
    assert!(
        mappings
            .iter()
            .all(|(_, reaper)| *reaper == subreaper.pid())
    );
    assert!(children.iter().all(|process| {
        process
            .parent()
            .is_some_and(|owner| Arc::ptr_eq(&owner, &subreaper))
    }));
}

#[test]
fn reparent_handoff_reselects_after_subreaper_toggle_between_batches() {
    let domain = domain();
    let init = init(&domain);
    let subreaper = child(&domain, &init, 2);
    domain
        .prepare_thread(&subreaper, 2)
        .unwrap()
        .commit()
        .unwrap();
    subreaper.set_child_subreaper(true);
    let parent = child(&domain, &subreaper, 3);
    let children: Vec<_> = (4..140).map(|pid| child(&domain, &parent, pid)).collect();

    let mut batches = 0;
    let mut mappings = Vec::new();
    domain
        .prepare_exit(&parent)
        .unwrap()
        .commit_with_reparent_handoff(zombie(3), drop, |batch| {
            batches += 1;
            for moved in batch.reparented() {
                let authoritative_parent = moved.child().parent().unwrap();
                assert!(Arc::ptr_eq(&authoritative_parent, batch.reaper()));
                mappings.push((moved.child().pid(), batch.reaper().pid()));
            }
            if batches == 1 {
                assert!(Arc::ptr_eq(batch.reaper(), &subreaper));
                subreaper.set_child_subreaper(false);
            } else {
                assert!(Arc::ptr_eq(batch.reaper(), &init));
            }
        });

    assert!(batches >= 2);
    assert_eq!(mappings.len(), children.len());
    assert!(
        mappings
            .iter()
            .any(|(_, reaper)| *reaper == subreaper.pid())
    );
    assert!(mappings.iter().any(|(_, reaper)| *reaper == init.pid()));
    for child in &children {
        let expected = mappings
            .iter()
            .find_map(|(pid, reaper)| (*pid == child.pid()).then_some(*reaper))
            .unwrap();
        assert_eq!(child.parent().unwrap().pid(), expected);
    }
}

#[test]
fn reparent_handoff_reselects_after_candidate_exit_between_batches() {
    let domain = domain();
    let init = init(&domain);
    domain.prepare_thread(&init, 1).unwrap().commit().unwrap();
    let subreaper = child(&domain, &init, 2);
    domain
        .prepare_thread(&subreaper, 2)
        .unwrap()
        .commit()
        .unwrap();
    subreaper.set_child_subreaper(true);
    let parent = child(&domain, &subreaper, 3);
    let children: Vec<_> = (4..140).map(|pid| child(&domain, &parent, pid)).collect();

    let mut exited_candidate = false;
    let mut mappings = Vec::new();
    domain
        .prepare_exit(&parent)
        .unwrap()
        .commit_with_reparent_handoff(zombie(3), drop, |batch| {
            for moved in batch.reparented() {
                let authoritative_parent = moved.child().parent().unwrap();
                assert!(Arc::ptr_eq(&authoritative_parent, batch.reaper()));
                mappings.push((moved.child().pid(), batch.reaper().pid()));
            }
            if !exited_candidate {
                assert!(Arc::ptr_eq(batch.reaper(), &subreaper));
                exited_candidate = true;
                let exit = match domain.exit_thread(&subreaper, 2, 0).unwrap() {
                    thekernel_linux_process::ThreadExitTransition::FinalThread(exit) => exit,
                    _ => panic!("candidate must publish a final-exit admission"),
                };
                assert_eq!(
                    exit.commit(zombie(2), drop).outcome(),
                    ExitOutcome::BecameZombie
                );
            } else {
                assert!(Arc::ptr_eq(batch.reaper(), &init));
            }
        });

    assert!(exited_candidate);
    assert!(subreaper.is_zombie());
    assert!(
        mappings
            .iter()
            .any(|(_, reaper)| *reaper == subreaper.pid())
    );
    assert!(mappings.iter().any(|(_, reaper)| *reaper == init.pid()));
    assert!(children.iter().all(|process| {
        process
            .parent()
            .is_some_and(|owner| Arc::ptr_eq(&owner, &init))
    }));
}

#[test]
fn thread_admissions_are_bounded_ordered_and_rollback_safe() {
    let domain = ProcessDomain::<Zombie>::try_with_membership_limit(3).unwrap();
    let init = init(&domain);

    let _ = domain
        .prepare_thread(&init, 30)
        .unwrap()
        .commit_infallible();
    domain.prepare_thread(&init, 10).unwrap().commit().unwrap();
    let pending = domain.prepare_thread(&init, 20).unwrap();
    assert_eq!(init.try_threads().unwrap(), [10, 30]);
    assert_eq!(
        domain.prepare_thread(&init, 40).err(),
        Some(ProcessError::Capacity)
    );
    drop(pending);
    domain.prepare_thread(&init, 20).unwrap().commit().unwrap();
    assert_eq!(init.thread_ids().collect::<Vec<_>>(), [10, 20, 30]);

    assert_eq!(init.exit_thread(999, 55), ThreadExitOutcome::NotFound);
    assert_eq!(init.exit_code(), 0);
    assert_eq!(
        init.exit_thread(10, 7),
        ThreadExitOutcome::LiveThreadsRemain
    );
    assert!(init.group_exit(42));
    assert!(!init.group_exit(99));
    assert_eq!(
        init.exit_thread(20, 3),
        ThreadExitOutcome::LiveThreadsRemain
    );
    assert_eq!(init.exit_thread(30, 3), ThreadExitOutcome::FinalThread);
    assert_eq!(init.exit_code(), 42);
    assert_eq!(domain.registry().thread_membership_count(), 0);
}

#[test]
fn init_is_unique_and_cannot_be_exited() {
    let domain = domain();
    let init = init(&domain);
    assert_eq!(
        domain.try_new_init(2, None).unwrap_err(),
        ProcessError::AlreadyExists
    );
    assert_eq!(
        domain.exit(&init, zombie(1), drop),
        Ok(ExitOutcome::InitProcess)
    );
    assert!(!init.is_zombie());
}

#[test]
fn domain_thread_limit_is_shared_across_processes() {
    let domain = ProcessDomain::<Zombie>::try_with_membership_limit(2).unwrap();
    let init = init(&domain);
    let child = child(&domain, &init, 2);

    domain.prepare_thread(&init, 10).unwrap().commit().unwrap();
    domain.prepare_thread(&child, 20).unwrap().commit().unwrap();
    assert_eq!(domain.registry().thread_membership_count(), 2);
    assert_eq!(
        domain.prepare_thread(&init, 30).err(),
        Some(ProcessError::Capacity)
    );
    assert!(child.remove_thread(20));
    domain.prepare_thread(&init, 30).unwrap().commit().unwrap();
    assert_eq!(domain.registry().thread_membership_count(), 2);
    assert!(init.remove_thread(10));
    assert!(init.remove_thread(30));
    exit_and_reap(&domain, &child);
}

#[test]
fn fork_admission_publishes_process_and_initial_thread_together() {
    let domain = domain();
    let init = init(&domain);
    let admission = domain
        .prepare_fork(&init, 2, None)
        .unwrap()
        .prepare_initial_thread(20)
        .unwrap();
    let reserved = admission.process().clone();

    assert!(domain.registry().get(2).is_none());
    let child = admission.commit();
    assert!(Arc::ptr_eq(&child, &reserved));
    assert!(Arc::ptr_eq(&domain.registry().get(2).unwrap(), &child));
    assert_eq!(child.thread_ids().collect::<Vec<_>>(), [20]);
    assert_eq!(child.exit_thread(20, 0), ThreadExitOutcome::FinalThread);
    exit_and_reap(&domain, &child);
}

#[test]
fn typed_initial_process_transaction_rolls_back_both_reservations() {
    let domain = domain();
    let init = init(&domain);
    let admission = domain
        .prepare_fork(&init, 2, None)
        .unwrap()
        .prepare_initial_thread(20)
        .unwrap();
    let child = admission.process().clone();

    assert_eq!(domain.registry().membership_count(), 2);
    assert_eq!(domain.registry().thread_membership_count(), 1);
    drop(admission);

    assert!(domain.registry().get(2).is_none());
    assert_eq!(domain.registry().membership_count(), 1);
    assert_eq!(domain.registry().thread_membership_count(), 0);
    assert_eq!(child.thread_count(), 0);
}

#[test]
fn unpublished_thread_token_cannot_escape_a_rolled_back_process_admission() {
    let domain = domain();
    let init = init(&domain);
    let admission = domain.prepare_fork(&init, 2, None).unwrap();
    let child = admission.process().clone();
    let thread = admission.prepare_thread(20).unwrap();
    drop(admission);

    assert_eq!(thread.commit(), Err(ProcessError::NotPublished));
    assert!(domain.registry().get(2).is_none());
    assert_eq!(child.thread_count(), 0);
    assert_eq!(domain.registry().thread_membership_count(), 0);
}

#[test]
fn pending_or_zombie_process_rejects_new_thread_publication() {
    let domain = domain();
    let init = init(&domain);
    let child = child(&domain, &init, 2);
    let pending = domain.prepare_thread(&child, 20).unwrap();

    assert_eq!(
        domain.exit(&child, zombie(2), drop),
        Err(ProcessError::NotLive)
    );
    drop(pending);
    assert_eq!(
        domain.exit(&child, zombie(2), drop),
        Ok(ExitOutcome::BecameZombie)
    );
    assert_eq!(
        domain.prepare_thread(&child, 21).err(),
        Some(ProcessError::NotLive)
    );
    assert!(domain.reap(&child).unwrap());
    assert_eq!(
        domain.prepare_thread(&child, 22).err(),
        Some(ProcessError::NotPublished)
    );
}

#[test]
fn final_thread_exit_rejects_an_unpublished_membership_without_stranding_process() {
    let domain = domain();
    let init = init(&domain);
    let process = domain
        .prepare_fork(&init, 2, None)
        .unwrap()
        .prepare_initial_thread(2)
        .unwrap()
        .commit();
    let pending = domain.prepare_thread(&process, 3).unwrap();

    assert!(matches!(
        domain.exit_thread(&process, 2, 0),
        Err(ProcessError::Busy)
    ));
    assert_eq!(process.thread_ids().collect::<Vec<_>>(), [2]);
    drop(pending);

    let exit = match domain.exit_thread(&process, 2, 0).unwrap() {
        thekernel_linux_process::ThreadExitTransition::FinalThread(exit) => exit,
        _ => panic!("last membership must prepare final exit"),
    };
    assert_eq!(
        exit.commit(zombie(2), drop).outcome(),
        ExitOutcome::BecameZombie
    );
    assert!(domain.reap(&process).unwrap());
}

#[test]
fn prepared_exit_excludes_competing_lifecycle_work_and_rolls_back() {
    let domain = domain();
    let init = init(&domain);
    let child = child(&domain, &init, 2);
    let exit = domain.prepare_exit(&child).unwrap();

    assert!(Arc::ptr_eq(exit.process(), &child));
    assert_eq!(
        domain.prepare_thread(&child, 20).err(),
        Some(ProcessError::NotLive)
    );
    assert_eq!(
        domain.exit(&child, zombie(2), drop),
        Err(ProcessError::Busy)
    );
    drop(exit);

    assert_eq!(
        domain.exit(&child, zombie(2), drop),
        Ok(ExitOutcome::BecameZombie)
    );
    assert!(domain.reap(&child).unwrap());
}
