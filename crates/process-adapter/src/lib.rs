//! TheKernel-specific payload adapter for `thekernel-linux-process`.
//!
//! The reusable crate keeps zombie state generic and requires an explicit
//! [`ProcessDomain`]. TheKernel records Linux wait status, CPU accounting, and
//! an immutable credential/namespace owner in that payload. The generic
//! credential parameter keeps this adapter below the kernel's `Cred` type
//! without reducing authority to a raw UID or a shadow registry.

#![no_std]
#![feature(allocator_api)]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(test)]
extern crate std;

use alloc::sync::Arc;
use core::mem::MaybeUninit;

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
///
/// `C` is the kernel's retained credential provenance. TheKernel uses an
/// immutable reference-counted credential which already owns its user
/// namespace; other consumers may choose an equivalent stable object.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ZombieSnapshot<C> {
    /// Linux wait status.
    pub wait_status: i32,
    /// CPU usage charged directly to the exited process.
    pub self_usage: ProcessUsage,
    /// CPU usage accumulated from already waited-for descendants.
    pub child_usage: ProcessUsage,
    /// Complete immutable credential and namespace provenance at exit.
    pub credential: C,
}

impl<C> ZombieSnapshot<C> {
    /// Creates the complete durable payload published by process exit.
    pub const fn new(
        wait_status: i32,
        self_usage: ProcessUsage,
        child_usage: ProcessUsage,
        credential: C,
    ) -> Self {
        Self {
            wait_status,
            self_usage,
            child_usage,
            credential,
        }
    }

    /// Returns the exited process and descendant usage total.
    pub fn total_usage(&self) -> ProcessUsage {
        self.self_usage.saturating_add(self.child_usage)
    }
}

/// Failure while reserving fixed-cost zombie snapshot storage.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum PreparedZombieSnapshotError {
    /// The reference-counted snapshot allocation could not be reserved.
    NoMemory,
}

/// Fallibly allocated storage for one immutable [`ZombieSnapshot`].
///
/// A process creates and owns this value while its runtime state is still
/// unpublished. [`initialize`](Self::initialize) consumes the sole owner,
/// writes the complete exit payload in place, and returns the same allocation
/// as `Arc<ZombieSnapshot<C>>`. No allocation occurs during that transition.
/// Dropping an uninitialized preparation simply releases its allocation; no
/// `C` value exists and no partially initialized payload is dropped.
#[must_use = "dropping prepared zombie storage releases it without publishing a snapshot"]
pub struct PreparedZombieSnapshot<C> {
    storage: Arc<MaybeUninit<ZombieSnapshot<C>>>,
}

impl<C> PreparedZombieSnapshot<C> {
    /// Allocates the reference-counted storage needed by the eventual exit
    /// snapshot, reporting OOM before process publication.
    pub fn try_new() -> Result<Self, PreparedZombieSnapshotError> {
        Self::try_new_with(|| Arc::try_new(MaybeUninit::uninit()))
    }

    fn try_new_with<E>(
        allocate: impl FnOnce() -> Result<Arc<MaybeUninit<ZombieSnapshot<C>>>, E>,
    ) -> Result<Self, PreparedZombieSnapshotError> {
        allocate()
            .map(|storage| Self { storage })
            .map_err(|_| PreparedZombieSnapshotError::NoMemory)
    }

    /// Completes the reserved payload and returns its immutable shared owner.
    ///
    /// This consumes the preparation, so a successful reservation can publish
    /// at most one snapshot. The operation is infallible: it only writes
    /// already-owned memory and changes the `Arc`'s pointee type after full
    /// initialization. It performs no allocation and does not clone
    /// `credential`.
    pub fn initialize(
        self,
        wait_status: i32,
        self_usage: ProcessUsage,
        child_usage: ProcessUsage,
        credential: C,
    ) -> Arc<ZombieSnapshot<C>> {
        let snapshot = ZombieSnapshot::new(wait_status, self_usage, child_usage, credential);
        let storage = Arc::into_raw(self.storage);

        // SAFETY:
        // - `storage` came directly from `Arc::into_raw`, and the private,
        //   non-`Clone` preparation never exposes the `Arc`, so this consuming
        //   method owns the only reference that can access the pointee;
        // - `MaybeUninit<ZombieSnapshot<C>>` has the same size and alignment as
        //   `ZombieSnapshot<C>`, and `write` initializes the entire value before
        //   the `Arc` is reconstructed with the initialized pointee type;
        // - no operation between `into_raw` and `from_raw` can panic, and the
        //   raw pointer is reconstructed exactly once.
        unsafe {
            storage
                .cast_mut()
                .cast::<ZombieSnapshot<C>>()
                .write(snapshot);
            Arc::from_raw(storage.cast::<ZombieSnapshot<C>>())
        }
    }

    /// Binds this process-owned reservation to a fully validated core exit
    /// transaction.
    ///
    /// The core token is obtained before this reservation is consumed. After
    /// binding, final payload initialization and zombie publication are one
    /// infallible consuming operation.
    pub fn bind_exit(self, exit: ProcessExitAdmission<C>) -> PreparedZombieExit<C> {
        PreparedZombieExit {
            storage: self,
            exit,
        }
    }
}

/// Type-bound prepared payload plus validated process-exit transaction.
pub struct PreparedZombieExit<C> {
    storage: PreparedZombieSnapshot<C>,
    exit: ProcessExitAdmission<C>,
}

impl<C> PreparedZombieExit<C> {
    /// Initializes the durable payload and publishes the zombie transition
    /// without allocation or a recoverable error.
    pub fn commit(
        self,
        wait_status: i32,
        self_usage: ProcessUsage,
        child_usage: ProcessUsage,
        credential: C,
        inherited_zombie: impl FnMut(Arc<Process<C>>),
    ) -> CommittedProcessExit<C> {
        let snapshot = self
            .storage
            .initialize(wait_status, self_usage, child_usage, credential);
        self.exit.commit(snapshot, inherited_zombie)
    }

    /// Initializes the durable payload, publishes the zombie transition, and
    /// forwards each authoritative core child-to-reaper batch after the core
    /// has released all of its topology guards.
    ///
    /// Different batches may name different reapers when a subreaper changes
    /// state between bounded core topology sections. Consumers must therefore
    /// use the identity supplied with each batch instead of independently
    /// repeating Linux reaper selection.
    pub fn commit_with_reparent_handoff(
        self,
        wait_status: i32,
        self_usage: ProcessUsage,
        child_usage: ProcessUsage,
        credential: C,
        inherited_zombie: impl FnMut(Arc<Process<C>>),
        reparent_batch: impl FnMut(&ProcessReparentBatch<C>),
    ) -> CommittedProcessExit<C> {
        let snapshot = self
            .storage
            .initialize(wait_status, self_usage, child_usage, credential);
        self.exit
            .commit_with_reparent_handoff(snapshot, inherited_zombie, reparent_batch)
    }
}

/// Process identifier, also used for thread, group, and session identifiers.
pub type Pid = thekernel_linux_process::Pid;

/// Largest Linux PID/TID representable through signed `pid_t` syscall ABIs.
///
/// TheKernel does not yet expose a mutable `pid_max` allocator policy, so this
/// is the hard Layer-2 identity ceiling rather than a claim that every value is
/// simultaneously allocatable.
pub const LINUX_PID_MAX: Pid = i32::MAX as Pid;

/// Failure while admitting a generic scheduler task identity into the Linux
/// PID/TID domain.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum LinuxTaskIdError {
    /// Zero is reserved for syscall-relative "current task" lookup semantics.
    Zero,
    /// The generic identity cannot be represented by the Linux PID type.
    OutOfRange,
}

/// Converts a generic monotonic task identity without truncation or PID-zero
/// publication.
///
/// Callers must perform this admission before publishing process, thread,
/// signal, scheduler, or lookup-table state. The generic allocator remains
/// free to use a wider identity domain for kernel-only tasks.
pub const fn try_pid_from_task_id(task_id: u64) -> Result<Pid, LinuxTaskIdError> {
    if task_id == 0 {
        Err(LinuxTaskIdError::Zero)
    } else if task_id > LINUX_PID_MAX as u64 {
        Err(LinuxTaskIdError::OutOfRange)
    } else {
        Ok(task_id as Pid)
    }
}

/// TheKernel process object parameterized by retained credential provenance.
pub type Process<C> = thekernel_linux_process::Process<ZombieSnapshot<C>>;
/// TheKernel process group parameterized by retained credential provenance.
pub type ProcessGroup<C> = thekernel_linux_process::ProcessGroup<ZombieSnapshot<C>>;
/// TheKernel session parameterized by retained credential provenance.
pub type Session<C> = thekernel_linux_process::Session<ZombieSnapshot<C>>;
/// TheKernel explicit process-domain owner.
pub type ProcessDomain<C> = thekernel_linux_process::ProcessDomain<ZombieSnapshot<C>>;
/// Read-only registry handle supplied by the explicit domain.
pub type ProcessRegistry<C> = thekernel_linux_process::ProcessRegistry<ZombieSnapshot<C>>;
/// Unpublished process admission transaction.
pub type ProcessAdmission<C> = thekernel_linux_process::ProcessAdmission<ZombieSnapshot<C>>;
/// Type-bound unpublished process plus initial-thread publication transaction.
pub type InitialProcessAdmission<C> =
    thekernel_linux_process::InitialProcessAdmission<ZombieSnapshot<C>>;
/// Fully validated final process-exit transaction.
pub type ProcessExitAdmission<C> = thekernel_linux_process::ProcessExitAdmission<ZombieSnapshot<C>>;
/// Completed zombie publication with its linearized notification parent.
pub type CommittedProcessExit<C> = thekernel_linux_process::CommittedProcessExit<ZombieSnapshot<C>>;
/// One authoritative bounded child-to-reaper handoff batch.
pub type ProcessReparentBatch<C> = thekernel_linux_process::ProcessReparentBatch<ZombieSnapshot<C>>;
/// One process moved by an authoritative reparent handoff batch.
pub type ReparentedProcess<C> = thekernel_linux_process::ReparentedProcess<ZombieSnapshot<C>>;
/// Domain-coordinated live-thread removal and final-exit admission result.
pub type ThreadExitTransition<C> = thekernel_linux_process::ThreadExitTransition<ZombieSnapshot<C>>;
/// Unpublished thread admission transaction.
pub type ThreadAdmission<C> = thekernel_linux_process::ThreadAdmission<ZombieSnapshot<C>>;
/// Ordered live-thread iterator.
pub type ThreadIds<C> = thekernel_linux_process::ThreadIds<ZombieSnapshot<C>>;
/// PID-ordered iterator over the explicit domain's published processes.
pub type Processes<'a, C> = thekernel_linux_process::Processes<'a, ZombieSnapshot<C>>;
/// Newly created session and process-group pair.
pub type CreatedSession<C> = thekernel_linux_process::CreatedSession<ZombieSnapshot<C>>;

pub use thekernel_linux_process::{
    ExitOutcome, PROCESS_MEMBERSHIP_LIMIT, ProcessError, ThreadExitOutcome,
    ThreadPublicationOutcome,
};

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        vec,
        vec::Vec,
    };

    use super::*;

    #[test]
    fn generic_task_identity_conversion_rejects_zero_and_truncation() {
        assert_eq!(try_pid_from_task_id(0), Err(LinuxTaskIdError::Zero));
        assert_eq!(try_pid_from_task_id(1), Ok(1));
        assert_eq!(
            try_pid_from_task_id(LINUX_PID_MAX as u64),
            Ok(LINUX_PID_MAX)
        );
        assert_eq!(
            try_pid_from_task_id(LINUX_PID_MAX as u64 + 1),
            Err(LinuxTaskIdError::OutOfRange)
        );
    }

    #[derive(Debug, Clone, Eq, PartialEq)]
    struct TestCredential {
        real_uid: u32,
        owner_user_ns: Arc<str>,
    }

    fn credential(real_uid: u32) -> TestCredential {
        TestCredential {
            real_uid,
            owner_user_ns: Arc::from("test-user-ns"),
        }
    }

    #[derive(Debug)]
    struct DropTrackedCredential {
        value: TestCredential,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for DropTrackedCredential {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn initialized_domain(
        limit: usize,
    ) -> (ProcessDomain<TestCredential>, Arc<Process<TestCredential>>) {
        let domain = ProcessDomain::try_with_membership_limit(limit).unwrap();
        let init = domain.try_new_init(1, None).unwrap();
        domain.prepare_thread(&init, 1).unwrap().commit().unwrap();
        (domain, init)
    }

    fn fork_with_initial_thread(
        domain: &ProcessDomain<TestCredential>,
        parent: &Arc<Process<TestCredential>>,
        pid: Pid,
    ) -> Arc<Process<TestCredential>> {
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
            credential(0),
        );
        assert_eq!(
            snapshot.total_usage(),
            ProcessUsage::with_maxrss(u64::MAX, 5, 13)
        );
    }

    #[test]
    fn prepared_snapshot_maps_allocator_failure_to_no_memory() {
        let result = PreparedZombieSnapshot::<TestCredential>::try_new_with(|| {
            Err::<Arc<MaybeUninit<ZombieSnapshot<TestCredential>>>, ()>(())
        });
        assert!(matches!(result, Err(PreparedZombieSnapshotError::NoMemory)));
    }

    #[test]
    fn prepared_snapshot_initializes_in_place_and_moves_credential_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let credential = DropTrackedCredential {
            value: credential(1000),
            drops: drops.clone(),
        };
        let prepared = PreparedZombieSnapshot::<DropTrackedCredential>::try_new().unwrap();
        let reserved =
            Arc::as_ptr(&prepared.storage).cast::<ZombieSnapshot<DropTrackedCredential>>();

        let snapshot = prepared.initialize(
            0x2a00,
            ProcessUsage::new(10, 20),
            ProcessUsage::new(30, 40),
            credential,
        );

        assert_eq!(Arc::as_ptr(&snapshot), reserved);
        assert_eq!(snapshot.credential.value.real_uid, 1000);
        assert_eq!(&*snapshot.credential.value.owner_user_ns, "test-user-ns");
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(snapshot);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_uninitialized_storage_drops_no_credential() {
        drop(PreparedZombieSnapshot::<DropTrackedCredential>::try_new().unwrap());
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
        let process = domain
            .prepare_fork(&init, 2, Some(17))
            .unwrap()
            .prepare_initial_thread(2)
            .unwrap();
        let child = process.process().clone();

        assert!(domain.registry().get(2).is_none());
        assert_eq!(child.thread_count(), 0);
        assert_eq!(domain.registry().membership_count(), 2);
        assert_eq!(domain.registry().thread_membership_count(), 2);

        let child = process.commit();
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

        let exit = domain.prepare_exit(&child).unwrap();
        let outcome = PreparedZombieSnapshot::try_new()
            .unwrap()
            .bind_exit(exit)
            .commit(
                child.exit_code(),
                ProcessUsage::new(10, 20),
                ProcessUsage::new(30, 40),
                credential(1000),
                drop,
            );
        assert_eq!(outcome.outcome(), ExitOutcome::BecameZombie);
        let first = child.zombie_payload().unwrap();
        let retained = first.clone();
        assert_eq!(retained.credential.real_uid, 1000);
        assert_eq!(&*retained.credential.owner_user_ns, "test-user-ns");

        let replacement = Arc::new(ZombieSnapshot::new(
            9,
            ProcessUsage::default(),
            ProcessUsage::default(),
            credential(0),
        ));
        assert_eq!(
            domain.exit(&child, replacement, drop),
            Ok(ExitOutcome::AlreadyZombie)
        );
        assert!(Arc::ptr_eq(&child.zombie_payload().unwrap(), &first));
        assert_eq!(domain.reap(&child), Ok(true));
        assert_eq!(domain.reap(&child), Ok(false));
        assert!(domain.registry().get(2).is_none());
    }

    #[test]
    fn prepared_exit_forwards_authoritative_reparent_handoff() {
        let (domain, init) = initialized_domain(8);
        let parent = fork_with_initial_thread(&domain, &init, 2);
        let child = fork_with_initial_thread(&domain, &parent, 3);
        let exit = match domain.exit_thread(&parent, 2, 0x2a00).unwrap() {
            ThreadExitTransition::FinalThread(exit) => exit,
            _ => panic!("single-thread parent must prepare final exit"),
        };

        let mut mapping = Vec::new();
        let outcome = PreparedZombieSnapshot::try_new()
            .unwrap()
            .bind_exit(exit)
            .commit_with_reparent_handoff(
                parent.exit_code(),
                ProcessUsage::new(10, 20),
                ProcessUsage::new(30, 40),
                credential(1000),
                drop,
                |batch| {
                    for moved in batch.reparented() {
                        mapping.push((moved.child().pid(), batch.reaper().pid()));
                    }
                },
            );

        assert_eq!(outcome.outcome(), ExitOutcome::BecameZombie);
        assert_eq!(mapping, [(child.pid(), init.pid())]);
        assert!(Arc::ptr_eq(&child.parent().unwrap(), &init));
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

        let processes: Processes<'_, TestCredential> = domain.registry().processes();
        assert_eq!(
            processes.map(|process| process.pid()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
