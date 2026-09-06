use std::sync::{Arc, Barrier};

use thekernel_linux_process::{
    ExitOutcome, Process, ProcessDomain, ThreadExitTransition, ThreadPublicationOutcome,
};

mod common;
use common::{child, domain, init, zombie};

fn live_child<Z>(domain: &ProcessDomain<Z>, parent: &Arc<Process<Z>>, pid: u32) -> Arc<Process<Z>> {
    domain
        .prepare_fork(parent, pid, Some(17))
        .unwrap()
        .prepare_initial_thread(pid)
        .unwrap()
        .commit()
}

fn commit_live_exit<Z>(domain: &ProcessDomain<Z>, process: &Arc<Process<Z>>, payload: Arc<Z>) {
    let exit = match domain
        .exit_thread(process, process.pid(), process.pid() as i32)
        .unwrap()
    {
        ThreadExitTransition::FinalThread(exit) => exit,
        _ => panic!("live single-thread process must produce a final-exit token"),
    };
    assert_eq!(
        exit.commit(payload, drop).outcome(),
        ExitOutcome::BecameZombie
    );
}

#[test]
fn concurrent_fork_admission_exit_and_reap_preserve_registry_counts() {
    const WORKERS: usize = 16;
    let domain = Arc::new(domain());
    let init = init(&domain);
    let start = Arc::new(Barrier::new(WORKERS));

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let worker_domain = domain.clone();
            let init = init.clone();
            let start = start.clone();
            scope.spawn(move || {
                let pid = worker as u32 + 2;
                start.wait();
                let process = child(&worker_domain, &init, pid);
                worker_domain.exit(&process, zombie(pid), drop).unwrap();
                assert!(worker_domain.reap(&process).unwrap());
            });
        }
    });

    assert_eq!(domain.registry().membership_count(), 1);
    assert_eq!(domain.registry().process_group_count(), 1);
    assert_eq!(init.group().membership_count(), 1);
}

#[test]
fn concurrent_thread_admission_and_removal_refund_domain_credits() {
    const WORKERS: usize = 32;
    let domain = Arc::new(domain());
    let init = init(&domain);
    let admitted = Arc::new(Barrier::new(WORKERS));

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let domain = domain.clone();
            let init = init.clone();
            let admitted = admitted.clone();
            scope.spawn(move || {
                let tid = worker as u32 + 100;
                domain.prepare_thread(&init, tid).unwrap().commit().unwrap();
                admitted.wait();
                assert!(init.remove_thread(tid));
            });
        }
    });

    assert_eq!(init.thread_count(), 0);
    assert_eq!(domain.registry().thread_membership_count(), 0);
}

#[test]
fn concurrent_thread_admission_and_exit_have_one_winner() {
    for round in 0..64 {
        let domain = Arc::new(domain());
        let init = init(&domain);
        let process = child(&domain, &init, 2);
        let start = Arc::new(Barrier::new(2));

        std::thread::scope(|scope| {
            let worker_domain = domain.clone();
            let worker_process = process.clone();
            let worker_start = start.clone();
            let admission = scope.spawn(move || {
                worker_start.wait();
                worker_domain
                    .prepare_thread(&worker_process, 100 + round)
                    .and_then(|thread| thread.commit())
            });

            start.wait();
            let exit = domain.exit(&process, zombie(2), drop);
            let admission = admission.join().unwrap();

            match (exit, admission) {
                (Ok(_), Err(thekernel_linux_process::ProcessError::NotLive)) => {
                    assert_eq!(process.thread_count(), 0)
                }
                (Err(thekernel_linux_process::ProcessError::NotLive), Ok(())) => {
                    assert_eq!(process.thread_count(), 1)
                }
                other => panic!("unexpected exit/admission result: {other:?}"),
            }
        });

        if process.thread_count() != 0 {
            assert!(process.remove_thread(100 + round));
            domain.exit(&process, zombie(2), drop).unwrap();
        }
        assert!(domain.reap(&process).unwrap());
    }
}

#[test]
fn group_exit_scan_or_late_publication_outcome_covers_every_thread() {
    for round in 0..64 {
        let domain = Arc::new(domain());
        let init = init(&domain);
        let process = live_child(&domain, &init, 2);
        let pending = domain.prepare_thread(&process, 100 + round).unwrap();
        let start = Arc::new(Barrier::new(2));

        let (outcome, scanned) = std::thread::scope(|scope| {
            let start_publish = start.clone();
            let publish = scope.spawn(move || {
                start_publish.wait();
                pending.commit_infallible()
            });
            start.wait();
            assert!(process.group_exit(9));
            let scanned: Vec<_> = process.thread_ids().collect();
            (publish.join().unwrap(), scanned)
        });

        match outcome {
            ThreadPublicationOutcome::Live => {
                assert!(scanned.contains(&(100 + round)));
            }
            ThreadPublicationOutcome::GroupExited => {
                // The kernel adapter queues SIGKILL to the exact prepared task
                // after TASK_TABLE publication and before runqueue insertion.
            }
        }
        assert!(process.remove_thread(2));
        assert!(process.remove_thread(100 + round));
        domain.exit(&process, zombie(2), drop).unwrap();
        assert!(domain.reap(&process).unwrap());
    }
}

#[test]
fn concurrent_nested_exit_keeps_leaf_on_the_nearest_subreaper() {
    for _ in 0..32 {
        let domain = Arc::new(domain());
        let init = init(&domain);
        domain.prepare_thread(&init, 1).unwrap().commit().unwrap();
        let subreaper = live_child(&domain, &init, 2);
        subreaper.set_child_subreaper(true);
        let ancestor = live_child(&domain, &subreaper, 3);
        let parent = live_child(&domain, &ancestor, 4);
        let leaf = live_child(&domain, &parent, 5);
        let start = Arc::new(Barrier::new(3));

        std::thread::scope(|scope| {
            let ancestor_domain = domain.clone();
            let ancestor = ancestor.clone();
            let start_ancestor = start.clone();
            scope.spawn(move || {
                start_ancestor.wait();
                commit_live_exit(&ancestor_domain, &ancestor, zombie(3));
            });

            let parent_domain = domain.clone();
            let parent = parent.clone();
            let start_parent = start.clone();
            scope.spawn(move || {
                start_parent.wait();
                commit_live_exit(&parent_domain, &parent, zombie(4));
            });

            start.wait();
        });

        assert!(ancestor.is_zombie());
        assert!(parent.is_zombie());
        assert!(Arc::ptr_eq(&leaf.parent().unwrap(), &subreaper));
        commit_live_exit(&domain, &leaf, zombie(5));
        assert!(domain.reap(&leaf).unwrap());
        assert!(domain.reap(&parent).unwrap());
        assert!(domain.reap(&ancestor).unwrap());
        commit_live_exit(&domain, &subreaper, zombie(2));
        assert!(domain.reap(&subreaper).unwrap());
    }
}
