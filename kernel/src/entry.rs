use alloc::{string::String, sync::Arc, vec::Vec};

use axfs::FS_CONTEXT;
use axhal::{power::system_off, uspace::UserContext};
use axsync::{Mutex, spin::SpinNoIrq};
use axtask::{
    AxTaskExt, SchedState, current, prepare_task_with_sched_from, publish_prepared_task,
    reserve_prepared_task,
};
use starry_process::Pid;

use crate::{
    file::{FD_TABLE, FdTable, executable, init_fd_scope_default, try_new_process_scope},
    mm::{
        copy_from_kernel, load_user_app_trusted, mark_page_fault_thread_context_ready,
        new_user_aspace_empty,
    },
    pseudofs::{self, dev::tty::N_TTY},
    task::{
        CgroupNamespace, Cred, CredentialSlot, Dumpability, NetworkNamespace, PidNamespace,
        ProcessAccessState, ProcessData, Thread, TimeNamespace, UserNamespace, UtsNamespace,
        init_process_domain, linux_pid_from_task_id, prepare_task_table_admission,
        spawn_alarm_task, try_new_user_task,
    },
};

/// Initialize and run initproc.
pub fn init(args: &[String], envs: &[String]) {
    const INIT_PID: Pid = 1;

    if let Err(error) = executable::init() {
        error!("failed to initialize bounded executable registry: {error}");
        system_off();
    }
    mark_page_fault_thread_context_ready();
    init_fd_scope_default().expect("Failed to initialize real fd scope default");

    {
        let fs = FS_CONTEXT.lock();
        let root = fs.root_dir();
        crate::mounts::register_linux_device(
            root.mountpoint().filesystem_identity(),
            crate::mounts::ROOT_BLOCK_DEVICE_ID,
        )
        .expect("Failed to register root block-device identity");
        crate::mounts::initialize_root_mount(
            root.mountpoint(),
            0,
            crate::mounts::MountMetadata::try_from_strs(
                crate::mounts::ROOT_BLOCK_SOURCE,
                root.filesystem().name(),
                "/",
                "",
            )
            .expect("Failed to allocate root mount metadata"),
        )
        .expect("Failed to initialize root mount policy");
    }
    axfs::set_symlink_follow_policy(crate::mounts::should_follow_symlink);
    axfs::set_atime_update_policy(crate::mounts::should_update_atime);
    axfs::set_mount_access_policy(crate::mounts::note_mount_access);
    crate::deferred_work::init();
    crate::file::inotify::init_filesystem_release_notifications();
    let security_registry = match crate::task::security::init() {
        Ok(registry) => registry,
        Err(error) => {
            error!("failed to initialize frozen security registry: {error}");
            system_off();
        }
    };
    let user_ns = UserNamespace::try_new_root().expect("Failed to allocate init user namespace");
    let root_cred = Cred::try_root_with_registry(security_registry, user_ns.clone())
        .expect("Failed to allocate init credential");
    let boot_credentials = root_cred.fs_dac_credentials();
    let credential =
        CredentialSlot::try_new(root_cred).expect("Failed to allocate init credential slot");
    let init_net_stack = axnet::default_stack().clone();
    pseudofs::mount_all(&boot_credentials, init_net_stack.unix_namespace())
        .expect("Failed to mount pseudofs");
    let init_net_ns = NetworkNamespace::try_new(init_net_stack, user_ns.clone())
        .expect("Failed to allocate init network namespace");

    let loc = FS_CONTEXT
        .lock()
        .resolve(&args[0])
        .expect("Failed to resolve executable path");
    let path = loc
        .absolute_path()
        .expect("Failed to get executable absolute path");
    let name = loc.name();

    let mut uspace = new_user_aspace_empty()
        .and_then(|mut it| {
            copy_from_kernel(&mut it)?;
            Ok(it)
        })
        .expect("Failed to create user address space");

    let (entry_vaddr, ustack_top) = load_user_app_trusted(&mut uspace, None, args, envs)
        .unwrap_or_else(|e| panic!("Failed to load user app: {}", e));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);
    let mut task_name = String::new();
    task_name
        .try_reserve_exact(name.len())
        .expect("Failed to allocate init task name");
    task_name.push_str(name);
    let mut task = try_new_user_task(task_name, uctx).expect("Failed to allocate init task");
    task.ctx_mut().set_page_table_root(uspace.page_table_root());

    let tid = linux_pid_from_task_id(task.id().as_u64())
        .expect("init task identity must fit the Linux PID domain");
    let process_domain = init_process_domain().expect("Failed to allocate process domain");
    let prepared_zombie_snapshot =
        ProcessData::try_prepare_zombie_snapshot().expect("Failed to reserve init zombie snapshot");
    let proc = process_domain
        .try_new_init(INIT_PID, None)
        .expect("Failed to allocate init process");

    N_TTY.bind_to(&proc).expect("Failed to bind ntty");

    let mut exe_path = String::new();
    exe_path
        .try_reserve_exact(path.as_str().len())
        .expect("Failed to allocate init executable path");
    exe_path.push_str(path.as_str());
    let mut cmdline = Vec::new();
    cmdline
        .try_reserve_exact(args.len())
        .expect("Failed to allocate init command line");
    for arg in args {
        let mut copy = String::new();
        copy.try_reserve_exact(arg.len())
            .expect("Failed to allocate init command-line argument");
        copy.push_str(arg);
        cmdline.push(copy);
    }
    let cmdline = Arc::try_new(cmdline).expect("Failed to allocate init command-line owner");
    let aspace = Arc::try_new(Mutex::new(uspace)).expect("Failed to allocate init address space");
    let access_state = ProcessAccessState::try_new(Dumpability::UserDumpable, user_ns.clone())
        .expect("Failed to allocate init process access state");
    let signal_actions =
        Arc::try_new(SpinNoIrq::new(Default::default())).expect("Failed to allocate init signals");
    let init_fd_table =
        Arc::try_new(FdTable::new().expect("Failed to allocate init fd-table identity"))
            .expect("Failed to allocate init fd table");
    let scope = try_new_process_scope(init_fd_table, FS_CONTEXT.clone())
        .expect("Failed to allocate init process scope");
    let exit_fd_table =
        Arc::try_new(FdTable::new().expect("Failed to allocate init exit fd-table identity"))
            .expect("Failed to allocate init exit fd table");
    let proc = ProcessData::try_new(
        proc,
        prepared_zombie_snapshot,
        credential.clone(),
        exe_path,
        executable::acquire(&loc).expect("Failed to retain init executable identity"),
        cmdline,
        aspace,
        access_state,
        scope,
        exit_fd_table,
        signal_actions,
        None,
        init_net_ns,
        CgroupNamespace::try_new_root(user_ns.clone())
            .expect("Failed to allocate init cgroup namespace"),
        PidNamespace::try_new_root(user_ns.clone()).expect("Failed to allocate init pid namespace"),
        UtsNamespace::try_new_root(user_ns.clone()).expect("Failed to allocate init UTS namespace"),
        TimeNamespace::try_new_root(user_ns).expect("Failed to allocate init time namespace"),
    )
    .expect("Failed to allocate init process runtime state");

    {
        let mut scope = proc.scope.write();
        crate::file::add_stdio(&FD_TABLE.scope_mut(&mut scope)).expect("Failed to add stdio");
    }

    let thread_admission = proc
        .prepare_thread(tid)
        .expect("Failed to admit init thread membership");
    let (thr, signal_registration) =
        Thread::try_new(tid, proc, credential).expect("Failed to allocate init thread state");
    if INIT_PID != tid {
        thr.set_tid(INIT_PID);
    }
    *task.task_ext_mut() = Some(AxTaskExt::from_impl(thr));

    let task = prepare_task_with_sched_from(task, SchedState::default(), &current())
        .expect("Failed to prepare init scheduler state");
    let task_publication =
        reserve_prepared_task(task.clone()).expect("Failed to reserve init runqueue publication");
    let task_table_admission =
        prepare_task_table_admission(&task).expect("Failed to reserve init lookup identities");
    signal_registration.commit();
    let thread_completion =
        task_table_admission.commit_with_publication(|| thread_admission.commit());
    let published_task = publish_prepared_task(task_publication);
    debug_assert!(Arc::ptr_eq(&published_task, &task));
    drop(published_task);
    thread_completion.finish();

    // Keep the init process user-visible as PID 1. Kernel-only alarm workers can
    // consume later scheduler task IDs without changing that ABI.
    spawn_alarm_task().expect("Failed to start alarm workers");

    // TODO: wait for all processes to finish
    let exit_code = task.join().expect("Failed to join init task");
    info!("Init process exited with code: {exit_code}");

    let cx = FS_CONTEXT.lock();
    cx.root_dir()
        .unmount_all()
        .expect("Failed to unmount all filesystems");
    cx.root_dir()
        .filesystem()
        .flush()
        .expect("Failed to flush rootfs");

    system_off();
}
