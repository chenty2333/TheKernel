use alloc::{string::String, sync::Arc, vec::Vec};

use axfs::FS_CONTEXT;
use axhal::{power::system_off, uspace::UserContext};
use axsync::{Mutex, spin::SpinNoIrq};
use axtask::{AxTaskExt, SchedState, spawn_task_with_sched};
use starry_process::{Pid, Process};

use crate::{
    file::{FD_TABLE, FdTable, executable, init_fd_scope_default, try_new_process_scope},
    mm::{
        copy_from_kernel, load_user_app_trusted, mark_page_fault_thread_context_ready,
        new_user_aspace_empty,
    },
    pseudofs::{self, dev::tty::N_TTY},
    task::{
        CgroupNamespace, Cred, PidNamespace, ProcessData, Thread, TimeNamespace, UserNamespace,
        UtsNamespace, add_task_to_table, spawn_alarm_task, try_new_user_task,
    },
};

/// Initialize and run initproc.
pub fn init(args: &[String], envs: &[String]) {
    const INIT_PID: Pid = 1;

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
    pseudofs::mount_all().expect("Failed to mount pseudofs");

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

    let tid = task.id().as_u64() as Pid;
    let proc = Process::try_new_init(INIT_PID, None).expect("Failed to allocate init process");

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
    let user_ns = UserNamespace::try_new_root().expect("Failed to allocate init user namespace");
    let credential = Cred::try_root(user_ns).expect("Failed to allocate init credential");
    let proc = ProcessData::try_new(
        proc,
        exe_path,
        executable::acquire(&loc).expect("Failed to retain init executable identity"),
        cmdline,
        aspace,
        scope,
        exit_fd_table,
        signal_actions,
        None,
        axnet::default_stack().clone(),
        CgroupNamespace::try_new_root().expect("Failed to allocate init cgroup namespace"),
        PidNamespace::try_new_root().expect("Failed to allocate init pid namespace"),
        Arc::try_new(UtsNamespace::new_default()).expect("Failed to allocate init UTS namespace"),
        Arc::try_new(TimeNamespace::new_default()).expect("Failed to allocate init time namespace"),
    )
    .expect("Failed to allocate init process runtime state");

    {
        let mut scope = proc.scope.write();
        crate::file::add_stdio(&mut FD_TABLE.scope_mut(&mut scope).write())
            .expect("Failed to add stdio");
    }

    let thread_admission = proc
        .prepare_thread(tid)
        .expect("Failed to admit init thread membership");
    let (thr, signal_registration) =
        Thread::try_new(tid, proc, credential).expect("Failed to allocate init thread state");
    signal_registration.commit();
    thread_admission.commit();
    if INIT_PID != tid {
        thr.set_tid(INIT_PID);
    }
    *task.task_ext_mut() = Some(AxTaskExt::from_impl(thr));

    // Keep the init process user-visible as PID 1. Kernel-only alarm workers can
    // consume later scheduler task IDs without changing that ABI.
    spawn_alarm_task();

    let task = spawn_task_with_sched(task, SchedState::default());
    add_task_to_table(&task).expect("Failed to publish init task lookup identities");

    // TODO: wait for all processes to finish
    let exit_code = task.join();
    info!("Init process exited with code: {exit_code:?}");

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
