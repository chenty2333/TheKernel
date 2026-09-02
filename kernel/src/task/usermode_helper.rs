//! Kernel-originated usermode helper construction.
//!
//! This is deliberately a process factory, not a kernel worker shortcut: a
//! helper has a normal PID, process-domain membership, signal endpoint,
//! namespace/fs/files snapshots and ordinary exit/reaper lifetime.  Callers
//! add their narrowly scoped authority only after this factory has published
//! the task identity.

use alloc::{format, string::String, sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::FsPathBuf;
use axhal::uspace::UserContext;
use axsync::Mutex;
use axtask::{
    AxTaskExt, AxTaskRef, SchedState, current, prepare_task_with_sched_from, publish_prepared_task,
    reserve_prepared_task,
};
use thekernel_linux_seccomp::SeccompState;
use thekernel_linux_signal::api::{SharedSignalActions, SignalActions};

use crate::{
    file::{
        FdTable, executable,
        permission::{VfsSecurityContext, check_pathwalk_search_permission_with_vfs_security},
        try_new_process_scope,
    },
    mm::{copy_from_kernel, load_user_app_helper, new_user_aspace_empty},
    task::{
        AsThread, CredentialSlot, Dumpability, FdTableSlot, ProcessAccessState, ProcessData,
        ProcessInitialAdmission, SchedulerSeed, TaskParentChoice, Thread, linux_pid_from_task_id,
        lock_task_parent_publication, prepare_task_table_admission, process_domain, process_error,
        set_task_user_address_space, try_new_user_task,
    },
};

const REQUEST_KEY_PATH: &[u8] = b"/sbin/request-key";

/// Published helper identity.  Cancellation is terminal and intentionally
/// does not grant any additional authority; normal Thread exit revokes the
/// one-shot construction key through the existing keyring lifecycle hook.
pub(crate) struct UsermodeHelperHandle {
    pub(crate) key_authority: Option<i32>,
    pub(crate) thread_owner: u32,
    task: AxTaskRef,
}

/// Fully captured launch input for a kernel-originated userspace process.
/// Additional helpers can reuse this factory without inheriting request-key
/// policy; `key_authority` is optional and is the sole subsystem-specific
/// capability installed in the post-identity/pre-runqueue publication gap.
pub(crate) struct UsermodeHelperSpec {
    pub(crate) path: FsPathBuf,
    pub(crate) arguments: Vec<Vec<u8>>,
    pub(crate) environment: Vec<Vec<u8>>,
    pub(crate) key_authority: Option<i32>,
}

impl UsermodeHelperHandle {
    pub(crate) fn cancel(&self) {
        self.task.as_thread().set_exit();
        self.task.interrupt();
    }
}

fn helper_arguments(
    serial: i32,
    kind: crate::keyring::KeyTypeKind,
    description: String,
    callout: String,
) -> AxResult<Vec<Vec<u8>>> {
    let values = [
        Vec::from(REQUEST_KEY_PATH),
        format!("{serial}").into_bytes(),
        Vec::from(kind.name().as_bytes()),
        description.into_bytes(),
        callout.into_bytes(),
    ];
    let mut arguments = Vec::new();
    arguments
        .try_reserve_exact(values.len())
        .map_err(|_| AxError::NoMemory)?;
    arguments.extend(values);
    Ok(arguments)
}

/// Launches `/sbin/request-key` from the caller's captured filesystem context.
/// The caller's namespaces and file table are snapshotted before any process
/// identity is published. Keyrings are intentionally not cloned; the caller
/// installs only `serial` authority after this function returns.
pub(crate) fn spawn_request_key_helper(
    serial: i32,
    kind: crate::keyring::KeyTypeKind,
    description: String,
    callout: String,
) -> AxResult<UsermodeHelperHandle> {
    spawn_usermode_helper(UsermodeHelperSpec {
        path: FsPathBuf::from_vec(Vec::from(REQUEST_KEY_PATH)),
        arguments: helper_arguments(serial, kind, description, callout)?,
        environment: Vec::new(),
        key_authority: Some(serial),
    })
}

/// Publishes a normal userspace process from an immutable caller snapshot.
/// It is the common primitive for kernel usermode helpers; callers supply all
/// executable and argument bytes up front so no user memory is consulted once
/// process construction begins.
pub(crate) fn spawn_usermode_helper(spec: UsermodeHelperSpec) -> AxResult<UsermodeHelperHandle> {
    let UsermodeHelperSpec {
        path: helper_path,
        arguments,
        environment: envs,
        key_authority,
    } = spec;
    let caller = current();
    let caller_thread = caller.as_thread();
    let parent = caller_thread.proc_data.clone();
    // Capture credentials, namespace proxy and fs_struct under one
    // publication gate. ProcessData only preserves the leader's construction
    // snapshot and is invalid after a sibling setns/unshare.
    let caller_snapshot = caller_thread.namespace_credential_fs_snapshot();
    let helper_fs = caller_snapshot.fs_slot.clone();
    let helper_files =
        Arc::try_new(caller_thread.fd_table().fork_copy()?).map_err(|_| AxError::NoMemory)?;
    let caller_cred = caller_snapshot.credential.clone();
    let helper_aux_identity = crate::task::ExecAuxIdentity::from_captured_ids(
        caller_cred.ids(),
        &caller_cred.user_ns().uid_map(),
        &caller_cred.user_ns().gid_map(),
    );

    // All executable pathname work uses this immutable submitter authority;
    // neither the initial lookup nor a script/PT_INTERP follow-up may sample
    // the kernel worker's credentials, mount topology, or Landlock domain.
    let helper_security = VfsSecurityContext::with_execution_authority(
        caller_cred.clone(),
        caller_snapshot.mount_topology.clone(),
        caller_snapshot.landlock_domain.clone(),
    );
    let loc =
        caller_snapshot
            .fs_snapshot
            .resolve_with_admission(&helper_path, &mut |directory| {
                check_pathwalk_search_permission_with_vfs_security(directory, &helper_security)
            })?;
    let executable = executable::acquire(&loc)?;
    let mut uspace = new_user_aspace_empty()?;
    copy_from_kernel(&mut uspace)?;
    let (entry, stack) = load_user_app_helper(
        &mut uspace,
        loc,
        &caller_snapshot.fs_snapshot,
        &helper_security,
        &helper_path,
        &arguments,
        &envs,
        helper_aux_identity,
    )?;
    let mut raw_task = try_new_user_task(
        String::from_utf8_lossy(REQUEST_KEY_PATH).into_owned(),
        UserContext::new(entry.into(), stack, 0),
    )?;
    set_task_user_address_space(raw_task.ctx_mut(), uspace.address_space_token());
    let tid = linux_pid_from_task_id(raw_task.id().as_u64())?;

    let helper_credential = CredentialSlot::try_new(caller_cred.clone())?;
    let mut helper_namespaces = caller_snapshot.namespaces;
    let pid_ns = helper_namespaces.pid_for_children();
    // A deferred PID namespace selected by unshare/setns becomes this
    // helper's current namespace as well as the namespace for its children.
    helper_namespaces.replace_pid(pid_ns.clone());
    helper_namespaces.replace_pid_for_children(pid_ns.clone());
    let _helper_ipc_ns = helper_namespaces.ipc();
    let helper_user_ns = helper_namespaces.user();
    let pid_namespace_init = pid_ns.has_no_init();
    let pid_reservation = pid_ns.reserve_process(tid)?;
    let reaper_scope = pid_ns.reaper_scope().ok_or(AxError::BadState)?;
    let domain = process_domain()?;
    let process_admission = if pid_namespace_init {
        ProcessInitialAdmission::ScopeInit(
            domain
                .prepare_fork_as_reaper_scope_init_with_identity(
                    &parent.proc,
                    &reaper_scope,
                    tid,
                    None,
                    pid_ns,
                )
                .map_err(process_error)?
                .prepare_initial_thread(tid)
                .map_err(process_error)?,
        )
    } else {
        ProcessInitialAdmission::Ordinary(
            domain
                .prepare_fork_in_reaper_scope_with_identity(
                    &parent.proc,
                    &reaper_scope,
                    tid,
                    None,
                    pid_ns,
                )
                .map_err(process_error)?
                .prepare_initial_thread(tid)
                .map_err(process_error)?,
        )
    };
    let process = process_admission.process().clone();
    let cmdline = Arc::try_new(arguments).map_err(|_| AxError::NoMemory)?;
    let aspace = Arc::try_new(Mutex::new(uspace)).map_err(|_| AxError::NoMemory)?;
    let access = ProcessAccessState::try_new(Dumpability::UserDumpable, helper_user_ns)?;
    let scope = try_new_process_scope()?;
    let exit_files = Arc::try_new(FdTable::new()?).map_err(|_| AxError::NoMemory)?;
    let actions =
        SharedSignalActions::try_new(SignalActions::default()).map_err(|_| AxError::NoMemory)?;
    let proc_data = ProcessData::try_new(
        process,
        ProcessData::try_prepare_zombie_snapshot()?,
        helper_credential.clone(),
        helper_path,
        executable,
        cmdline,
        aspace,
        access,
        scope,
        exit_files,
        actions,
        None,
        helper_namespaces,
    )?;
    let thread_admission = proc_data.prepare_initial_thread_admission(process_admission)?;
    let cgroup_admission =
        crate::pseudofs::cgroup::prepare_fork_charge(parent.proc.pid(), tid, &proc_data.proc)?;

    // `files_struct` is a fork snapshot, whereas `fs_struct` intentionally
    // shares the captured root/cwd identity. Both are acquired before task
    // publication and are released by ordinary Thread teardown.
    let (thread, signal_registration) = Thread::try_new(
        tid,
        proc_data.clone(),
        helper_credential,
        Arc::new(SeccompState::disabled()),
        helper_fs,
        FdTableSlot::new(helper_files),
        SchedulerSeed {
            state: SchedState::default(),
            reset_on_fork: false,
            uclamp: axtask::UclampRequest::unrestricted(),
            utilization_bounds: axtask::UtilizationBounds::unrestricted(),
            version: 0,
        },
    )?;
    proc_data.bind_initial_group_leader_signal(
        tid,
        thread.signal.clone(),
        thread.landlock_domain(),
    )?;
    *raw_task.task_ext_mut() = Some(AxTaskExt::from_impl(thread));
    let task = prepare_task_with_sched_from(raw_task, SchedState::default(), &caller)?;
    let task_publication =
        reserve_prepared_task(task.clone()).map_err(|error| error.into_ax_error())?;
    let task_table_admission = prepare_task_table_admission(&task)?;

    // No fallible work remains after this point. Publish the normal task
    // parent, signal endpoint, process/thread core identity and PID binding
    // in the same order used by clone, then make it scheduler-runnable.
    let task_parent_publication = lock_task_parent_publication();
    task.as_thread().publish_task_parent(
        &task_parent_publication,
        TaskParentChoice::Caller(caller_thread.task_parent_node().clone()),
    );
    signal_registration
        .commit()
        .expect("private request-key helper signal registration was cancelled");
    let (_process, thread_completion) =
        task_table_admission.commit_with_publication(|| thread_admission.commit());
    pid_reservation.commit();
    drop(task_parent_publication);
    cgroup_admission.commit();
    // TASK_TABLE and process-domain identity now exist, but the task has not
    // reached a runqueue. Install the one-shot key authority in precisely this
    // gap so `/sbin/request-key` cannot race its own capability publication.
    if let Some(serial) = key_authority
        && let Err(error) = crate::keyring::install_request_key_authority(serial, tid)
    {
        task.as_thread().set_exit();
        let published = publish_prepared_task(task_publication);
        drop(published);
        thread_completion.finish();
        return Err(error);
    }
    let published = publish_prepared_task(task_publication);
    debug_assert!(Arc::ptr_eq(&published, &task));
    drop(published);
    thread_completion.finish();
    Ok(UsermodeHelperHandle {
        key_authority,
        thread_owner: tid,
        task,
    })
}
