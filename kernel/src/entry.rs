use alloc::{
    string::{String, ToString},
    sync::Arc,
};

use axfs::FS_CONTEXT;
use axhal::{power::system_off, uspace::UserContext};
use axsync::Mutex;
use axtask::{AxTaskExt, SchedState, spawn_task_with_sched};
use starry_process::{Pid, Process};

use crate::{
    file::FD_TABLE,
    mm::{
        copy_from_kernel, load_user_app, mark_page_fault_thread_context_ready,
        new_user_aspace_empty,
    },
    pseudofs::{self, dev::tty::N_TTY},
    task::{ProcessData, Thread, add_task_to_table, new_user_task, spawn_alarm_task},
};

/// Initialize and run initproc.
pub fn init(args: &[String], envs: &[String]) {
    const INIT_PID: Pid = 1;

    mark_page_fault_thread_context_ready();

    axfs::set_symlink_follow_policy(crate::mounts::should_follow_symlink);
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

    let (entry_vaddr, ustack_top) = load_user_app(&mut uspace, None, args, envs)
        .unwrap_or_else(|e| panic!("Failed to load user app: {}", e));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);
    let mut task = new_user_task(name, uctx);
    task.ctx_mut().set_page_table_root(uspace.page_table_root());

    let tid = task.id().as_u64() as Pid;
    let proc = Process::new_init(INIT_PID, None);
    proc.add_thread(tid);

    N_TTY.bind_to(&proc).expect("Failed to bind ntty");

    let proc = ProcessData::new(
        proc,
        path.to_string(),
        Arc::new(args.to_vec()),
        Arc::new(Mutex::new(uspace)),
        Arc::default(),
        None,
        axnet::default_stack().clone(),
    );

    {
        let mut scope = proc.scope.write();
        crate::file::add_stdio(&mut FD_TABLE.scope_mut(&mut scope).write())
            .expect("Failed to add stdio");
    }

    let thr = Thread::new(tid, proc);
    if INIT_PID != tid {
        thr.set_tid(INIT_PID);
    }
    *task.task_ext_mut() = Some(AxTaskExt::from_impl(thr));

    // Keep the init process user-visible as PID 1. Kernel-only alarm workers can
    // consume later scheduler task IDs without changing that ABI.
    spawn_alarm_task();

    let task = spawn_task_with_sched(task, SchedState::default());
    add_task_to_table(&task);

    // TODO: wait for all processes to finish
    let exit_code = task.join();
    info!("Init process exited with code: {exit_code:?}");

    // Flush dirty page caches before unmount so that writeback can still
    // reach the filesystem.
    let _ = axfs::sync_all_page_caches();

    let cx = FS_CONTEXT.lock();
    cx.root_dir()
        .unmount_all()
        .expect("Failed to unmount all filesystems");
    cx.root_dir()
        .filesystem()
        .flush()
        .expect("Failed to flush rootfs");
    drop(cx);

    system_off();
}
