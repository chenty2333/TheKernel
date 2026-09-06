use std::sync::Arc;

use thekernel_linux_process::{Process, ProcessDomain, ProcessError, ThreadExitOutcome};

mod common;
use common::{Zombie, domain, exit_and_reap, init, zombie};

fn live_child(
    domain: &ProcessDomain<Zombie>,
    parent: &Arc<Process<Zombie>>,
    pid: u32,
) -> Arc<Process<Zombie>> {
    domain
        .prepare_fork(parent, pid, Some(17))
        .unwrap()
        .prepare_initial_thread(pid)
        .unwrap()
        .commit()
}

fn exit_live_and_reap(domain: &ProcessDomain<Zombie>, process: &Arc<Process<Zombie>>) {
    assert_eq!(
        process.exit_thread(process.pid(), 0),
        ThreadExitOutcome::FinalThread
    );
    exit_and_reap(domain, process);
}

#[test]
fn group_membership_uses_an_explicit_registry() {
    let process_domain = domain();
    let other_domain = domain();
    let init_process = init(&process_domain);
    let other_init = init(&other_domain);
    let group = init_process.group();
    let child = live_child(&process_domain, &init_process, 2);

    let processes = group.try_processes(process_domain.registry()).unwrap();
    assert_eq!(processes.len(), 2);
    assert!(
        processes
            .iter()
            .any(|process| Arc::ptr_eq(process, &init_process))
    );
    assert!(processes.iter().any(|process| Arc::ptr_eq(process, &child)));
    assert_eq!(
        group.try_processes(other_domain.registry()).err(),
        Some(ProcessError::WrongDomain)
    );
    assert!(
        other_init
            .group()
            .try_processes(other_domain.registry())
            .is_ok()
    );

    let mut visited = Vec::new();
    group
        .for_each_process(process_domain.registry(), |process| {
            visited.push(process.pid())
        })
        .unwrap();
    assert_eq!(visited, [1, 2]);
    assert!(
        group
            .any_process(process_domain.registry(), |process| process.pid() == 2)
            .unwrap()
    );
    exit_live_and_reap(&process_domain, &child);
}

#[test]
fn create_and_move_group_preserve_session_identity() {
    let process_domain = domain();
    let init_process = init(&process_domain);
    let first = live_child(&process_domain, &init_process, 2);
    let first_group = process_domain.try_create_group(&first).unwrap().unwrap();
    let second = live_child(&process_domain, &init_process, 3);
    let second_group = process_domain.try_create_group(&second).unwrap().unwrap();

    assert!(process_domain.move_to_group(&second, &first_group).unwrap());
    assert!(Arc::ptr_eq(&second.group(), &first_group));
    assert_eq!(
        first_group
            .try_processes(process_domain.registry())
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        second_group.try_processes(process_domain.registry()).err(),
        Some(ProcessError::NotPublished)
    );

    let session_candidate = live_child(&process_domain, &init_process, 4);
    let (new_session, new_group) = process_domain
        .try_create_session(&session_candidate)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&new_group.session(), &new_session));
    assert!(!process_domain.move_to_group(&second, &new_group).unwrap());

    exit_live_and_reap(&process_domain, &session_candidate);
    assert!(!new_group.is_live());
    assert!(!new_session.is_live());
    assert_eq!(
        new_session
            .try_process_groups(process_domain.registry())
            .err(),
        Some(ProcessError::NotPublished)
    );
    exit_live_and_reap(&process_domain, &second);
    exit_live_and_reap(&process_domain, &first);
}

#[test]
fn retired_group_arcs_cannot_be_revived_when_pgid_is_reused() {
    let process_domain = domain();
    let init_process = init(&process_domain);
    let child = live_child(&process_domain, &init_process, 2);
    let init_group = init_process.group();
    let retired = process_domain.try_create_group(&child).unwrap().unwrap();

    assert!(process_domain.move_to_group(&child, &init_group).unwrap());
    assert!(!retired.is_live());
    assert!(process_domain.registry().get_process_group(2).is_none());
    assert_eq!(
        retired.try_processes(process_domain.registry()).err(),
        Some(ProcessError::NotPublished)
    );
    assert_eq!(
        process_domain.move_to_group(&child, &retired).err(),
        Some(ProcessError::NotPublished)
    );

    let replacement = process_domain.try_create_group(&child).unwrap().unwrap();
    assert_eq!(replacement.pgid(), retired.pgid());
    assert!(!Arc::ptr_eq(&replacement, &retired));
    assert!(replacement.is_live());
    assert!(Arc::ptr_eq(
        &process_domain.registry().get_process_group(2).unwrap(),
        &replacement
    ));
    exit_live_and_reap(&process_domain, &child);
}

#[test]
fn zombie_process_cannot_mutate_job_control_membership() {
    let process_domain = domain();
    let init_process = init(&process_domain);
    let child = live_child(&process_domain, &init_process, 2);
    let group = init_process.group();

    assert_eq!(child.exit_thread(2, 0), ThreadExitOutcome::FinalThread);
    assert_eq!(
        process_domain.try_create_group(&child).err(),
        Some(ProcessError::NotLive)
    );
    let prepared = process_domain.prepare_exit(&child).unwrap();
    assert_eq!(
        process_domain.move_to_group(&child, &group),
        Err(ProcessError::NotLive)
    );
    drop(prepared);
    process_domain.exit(&child, zombie(2), drop).unwrap();
    assert_eq!(
        process_domain.try_create_group(&child).err(),
        Some(ProcessError::NotLive)
    );
    assert_eq!(
        process_domain.try_create_session(&child).err(),
        Some(ProcessError::NotLive)
    );
    assert_eq!(
        process_domain.move_to_group(&child, &group),
        Err(ProcessError::NotLive)
    );
    assert!(process_domain.reap(&child).unwrap());
}
