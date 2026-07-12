//! TheKernel-specific payload adapter for `thekernel-linux-process`.
//!
//! The reusable crate keeps zombie state generic and requires an explicit
//! [`ProcessDomain`]. TheKernel records Linux wait status, CPU accounting, and
//! the exiting real UID in that payload. These aliases preserve one concrete
//! type across the kernel without reintroducing a global registry or a second
//! lifecycle state machine.

#![no_std]
#![deny(missing_docs)]

#[cfg(test)]
extern crate std;

/// Durable CPU usage totals for a process subtree.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ProcessUsage {
    /// User CPU time in nanoseconds.
    pub utime_ns: u64,
    /// System CPU time in nanoseconds.
    pub stime_ns: u64,
    /// Maximum resident set size in kilobytes.
    pub maxrss_kb: u64,
}

impl ProcessUsage {
    /// Creates a usage record without a resident-set high-water value.
    pub const fn new(utime_ns: u64, stime_ns: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb: 0,
        }
    }

    /// Creates a complete usage record.
    pub const fn with_maxrss(utime_ns: u64, stime_ns: u64, maxrss_kb: u64) -> Self {
        Self {
            utime_ns,
            stime_ns,
            maxrss_kb,
        }
    }

    /// Adds CPU time with explicit saturation and retains the larger RSS
    /// high-water mark.
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            utime_ns: self.utime_ns.saturating_add(other.utime_ns),
            stime_ns: self.stime_ns.saturating_add(other.stime_ns),
            maxrss_kb: self.maxrss_kb.max(other.maxrss_kb),
        }
    }
}

/// Immutable Linux-visible state retained after runtime process data is gone.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ZombieSnapshot {
    /// Linux wait status.
    pub wait_status: i32,
    /// CPU usage charged directly to the exited process.
    pub self_usage: ProcessUsage,
    /// CPU usage accumulated from already waited-for descendants.
    pub child_usage: ProcessUsage,
    /// Real UID of the exiting process in TheKernel's current ABI snapshot.
    pub uid: u32,
}

impl ZombieSnapshot {
    /// Creates the complete durable payload published by process exit.
    pub const fn new(
        wait_status: i32,
        self_usage: ProcessUsage,
        child_usage: ProcessUsage,
        uid: u32,
    ) -> Self {
        Self {
            wait_status,
            self_usage,
            child_usage,
            uid,
        }
    }

    /// Returns the exited process and descendant usage total.
    pub fn total_usage(self) -> ProcessUsage {
        self.self_usage.saturating_add(self.child_usage)
    }
}

/// Process identifier, also used for thread, group, and session identifiers.
pub type Pid = process_core::Pid;
/// TheKernel's concrete process object.
pub type Process = process_core::Process<ZombieSnapshot>;
/// TheKernel's concrete process group.
pub type ProcessGroup = process_core::ProcessGroup<ZombieSnapshot>;
/// TheKernel's concrete session.
pub type Session = process_core::Session<ZombieSnapshot>;
/// TheKernel's sole explicit process-domain owner.
pub type ProcessDomain = process_core::ProcessDomain<ZombieSnapshot>;
/// Read-only registry handle supplied by the explicit domain.
pub type ProcessRegistry = process_core::ProcessRegistry<ZombieSnapshot>;
/// Unpublished process admission transaction.
pub type ProcessAdmission = process_core::ProcessAdmission<ZombieSnapshot>;
/// Unpublished thread admission transaction.
pub type ThreadAdmission = process_core::ThreadAdmission<ZombieSnapshot>;
/// Ordered live-thread iterator.
pub type ThreadIds = process_core::ThreadIds<ZombieSnapshot>;
/// PID-ordered iterator over the explicit domain's published processes.
pub type Processes<'a> = process_core::Processes<'a, ZombieSnapshot>;
/// Newly created session and process-group pair.
pub type CreatedSession = process_core::CreatedSession<ZombieSnapshot>;

pub use process_core::{ExitOutcome, PROCESS_MEMBERSHIP_LIMIT, ProcessError, ThreadExitOutcome};

#[cfg(test)]
mod tests {
    use std::{sync::Arc, vec, vec::Vec};

    use super::*;

    fn initialized_domain(limit: usize) -> (ProcessDomain, Arc<Process>) {
        let domain = ProcessDomain::try_with_membership_limit(limit).unwrap();
        let init = domain.try_new_init(1, None).unwrap();
        domain.prepare_thread(&init, 1).unwrap().commit().unwrap();
        (domain, init)
    }

    fn fork_with_initial_thread(
        domain: &ProcessDomain,
        parent: &Arc<Process>,
        pid: Pid,
    ) -> Arc<Process> {
        let process = domain.prepare_fork(parent, pid, Some(17)).unwrap();
        let thread = process.prepare_thread(pid).unwrap();
        process.commit_with_thread(thread).unwrap()
    }

    #[test]
    fn zombie_payload_preserves_saturating_cpu_and_peak_memory_semantics() {
        let snapshot = ZombieSnapshot::new(
            0,
            ProcessUsage::with_maxrss(u64::MAX, 2, 8),
            ProcessUsage::with_maxrss(1, 3, 13),
            0,
        );
        assert_eq!(
            snapshot.total_usage(),
            ProcessUsage::with_maxrss(u64::MAX, 5, 13)
        );
    }

    #[test]
    fn domains_are_explicit_and_may_reuse_the_same_pid() {
        let (first, first_init) = initialized_domain(4);
        let (second, second_init) = initialized_domain(4);

        assert_eq!(first_init.pid(), second_init.pid());
        assert!(!Arc::ptr_eq(&first_init, &second_init));
        assert!(Arc::ptr_eq(&first.registry().get(1).unwrap(), &first_init));
        assert!(Arc::ptr_eq(
            &second.registry().get(1).unwrap(),
            &second_init
        ));
        assert_eq!(first.registry().membership_count(), 1);
        assert_eq!(second.registry().membership_count(), 1);
    }

    #[test]
    fn initial_thread_and_process_publish_in_one_commit() {
        let (domain, init) = initialized_domain(4);
        let process = domain.prepare_fork(&init, 2, Some(17)).unwrap();
        let child = process.process().clone();
        let thread = process.prepare_thread(2).unwrap();

        assert!(domain.registry().get(2).is_none());
        assert_eq!(child.thread_count(), 0);
        assert_eq!(domain.registry().membership_count(), 2);
        assert_eq!(domain.registry().thread_membership_count(), 2);

        let child = process.commit_with_thread(thread).unwrap();
        assert!(Arc::ptr_eq(&domain.registry().get(2).unwrap(), &child));
        assert_eq!(child.thread_count(), 1);
    }

    #[test]
    fn dropped_fork_reservations_refund_every_capacity_charge() {
        let (domain, init) = initialized_domain(3);
        {
            let process = domain.prepare_fork(&init, 2, None).unwrap();
            let _thread = process.prepare_thread(2).unwrap();
            assert_eq!(domain.registry().membership_count(), 2);
            assert_eq!(domain.registry().thread_membership_count(), 2);
        }

        assert!(domain.registry().get(2).is_none());
        assert_eq!(domain.registry().membership_count(), 1);
        assert_eq!(domain.registry().thread_membership_count(), 1);
        assert!(fork_with_initial_thread(&domain, &init, 2).is_live());
    }

    #[test]
    fn exit_publishes_one_snapshot_and_reap_is_typed_and_idempotent() {
        let (domain, init) = initialized_domain(4);
        let child = fork_with_initial_thread(&domain, &init, 2);
        assert_eq!(child.exit_thread(2, 0x2a00), ThreadExitOutcome::FinalThread);

        let first = ZombieSnapshot::new(
            child.exit_code(),
            ProcessUsage::new(10, 20),
            ProcessUsage::new(30, 40),
            1000,
        );
        assert_eq!(
            domain.exit(&child, first, drop),
            Ok(ExitOutcome::BecameZombie)
        );
        assert_eq!(child.zombie_payload(), Some(first));

        let replacement =
            ZombieSnapshot::new(9, ProcessUsage::default(), ProcessUsage::default(), 0);
        assert_eq!(
            domain.exit(&child, replacement, drop),
            Ok(ExitOutcome::AlreadyZombie)
        );
        assert_eq!(child.zombie_payload(), Some(first));
        assert_eq!(domain.reap(&child), Ok(true));
        assert_eq!(domain.reap(&child), Ok(false));
        assert!(domain.registry().get(2).is_none());
    }

    #[test]
    fn topology_queries_and_job_control_use_the_domain_registry() {
        let (domain, init) = initialized_domain(8);
        let second = fork_with_initial_thread(&domain, &init, 2);
        let third = fork_with_initial_thread(&domain, &init, 3);

        let group = domain.try_create_group(&second).unwrap().unwrap();
        assert!(domain.move_to_group(&third, &group).unwrap());
        let members = group.try_processes(domain.registry()).unwrap();
        assert_eq!(
            members
                .iter()
                .map(|process| process.pid())
                .collect::<Vec<_>>(),
            vec![2, 3]
        );

        let session = group.session();
        let groups = session.try_process_groups(domain.registry()).unwrap();
        assert_eq!(
            groups.iter().map(|group| group.pgid()).collect::<Vec<_>>(),
            vec![1, 2]
        );

        let processes: Processes<'_> = domain.registry().processes();
        assert_eq!(
            processes.map(|process| process.pid()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
