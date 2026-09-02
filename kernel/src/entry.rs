use alloc::{string::String, sync::Arc, vec::Vec};

use axfs::FS_CONTEXT;
use axfs_ng_vfs::{FsPath, FsPathBuf};
use axhal::{power::system_off, uspace::UserContext};
use axsync::Mutex;
use axtask::{
    AxTaskExt, SchedState, current, prepare_task_with_sched_from, publish_prepared_task,
    reserve_prepared_task,
};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_seccomp::SeccompState;
use thekernel_linux_signal::api::{SharedSignalActions, SignalActions};

use crate::{
    file::{FdTable, executable, try_new_process_scope},
    mm::{copy_from_kernel, load_user_app_trusted, new_user_aspace_empty},
    pseudofs::{
        self,
        dev::tty::{N_TTY, VT_MANAGER},
    },
    task::{
        CgroupNamespace, Cred, CredentialSlot, Dumpability, FsContextSlot, MountNamespace,
        NamespaceProxy, NetworkNamespace, PidNamespace, ProcessAccessState, ProcessData,
        SchedulerSeed, Thread, TimeNamespace, UserNamespace, UtsNamespace, init_process_domain,
        init_seccomp_filter_budget, linux_pid_from_task_id, prepare_task_table_admission,
        set_task_user_address_space, spawn_alarm_task, try_new_user_task,
    },
};

/// Initialize and run initproc.
pub fn init(args: &[String], envs: &[String]) {
    const INIT_PID: Pid = 1;

    crate::syscall::init_crash_kexec_hook();
    crate::mm::init_tlb_shootdown();
    crate::syscall::init_membarrier_ipi();
    #[cfg(feature = "perf-sampling")]
    assert!(
        crate::file::PerfSampleBackend::init_nmi(),
        "failed to initialize the PMU sampling NMI path"
    );
    #[cfg(all(feature = "perf-sampling", target_os = "none"))]
    assert!(
        crate::file::perf::init_reconcile_ipi(),
        "failed to reserve the perf reconciliation IPI lane"
    );
    #[cfg(all(feature = "smp-tlb-shootdown", target_os = "none"))]
    axtask::init_remote_resched_ipi()
        .expect("failed to register the EEVDF remote-reschedule IPI consumer");
    #[cfg(all(feature = "hwp-uclamp", target_os = "none"))]
    if axtask::init_hwp_clamp_refresh_ipi().is_err() {
        // HWP itself remains fleet-safe and the scheduler switch/tick paths
        // remain authoritative.  The IPI is only prompt propagation, so an
        // occupied optional broker lane must not prevent boot.
        warn!("HWP clamp-refresh IPI unavailable; using switch/tick refresh");
    }
    crate::mm::init_hardware_asids();
    crate::rcu::init().expect("Failed to initialize kernel RCU domains");
    init_seccomp_filter_budget().expect("Failed to initialize bounded seccomp filter budget");
    if let Err(error) = crate::jit_memory::init() {
        // Native cBPF and native ET_REL modules share this strictly W^X
        // arena. Their optional admission paths return the captured error;
        // boot must not weaken alias safety when reservation fails.
        error!("native executable arena unavailable: {error:?}");
    }
    if let Err(error) = crate::syscall::init_kernel_module_exports() {
        // Module relocation has a single, explicit native ABI.  Do not boot
        // with an empty or partially published export registry, because that
        // would make later module admission depend on boot ordering.
        error!("failed to publish native module ABI exports: {error}");
        system_off();
    }
    if let Err(error) = executable::init() {
        error!("failed to initialize bounded executable registry: {error}");
        system_off();
    }

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
            crate::mounts::MountMetadata::try_from_parts(
                axfs_ng_vfs::FsPath::new(crate::mounts::ROOT_BLOCK_SOURCE.as_bytes()),
                root.filesystem().name(),
                axfs_ng_vfs::FsPath::new(b"/"),
                "",
            )
            .expect("Failed to allocate root mount metadata"),
        )
        .expect("Failed to initialize root mount policy");
    }
    axfs::set_symlink_follow_policy(crate::mounts::should_follow_symlink);
    axfs::set_atime_update_policy(crate::mounts::should_update_atime);
    axfs::set_mount_access_policy(crate::mounts::note_mount_access);
    axfs::set_automount_trigger_policy(crate::mounts::trigger_automount);
    crate::deferred_work::init();
    crate::mm::init_memory_pressure();
    crate::mm::init_khugepaged();
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
    let boot_security = crate::file::permission::VfsSecurityContext::new(root_cred.clone());
    let credential =
        CredentialSlot::try_new(root_cred).expect("Failed to allocate init credential slot");
    let init_net_stack = axnet::default_stack().clone();
    let init_net_ns = NetworkNamespace::try_new(init_net_stack.clone(), user_ns.clone())
        .expect("Failed to allocate init network namespace");
    crate::file::netlink::register_init_network_namespace(&init_net_ns)
        .expect("Failed to register init network namespace for device uevents");
    // Device publication below (including DRM's initial registration) may
    // emit uevents, so init-net must be established first.
    match crate::drm::init_virtio_gpu() {
        Ok(true) => info!("registered VirtIO GPU as DRM primary device"),
        Ok(false) => info!("no DRM-capable VirtIO GPU found"),
        Err(error) => error!("failed to initialize DRM VirtIO GPU: {error}"),
    }
    {
        let fs = FS_CONTEXT.lock();
        pseudofs::mount_all(&fs, &boot_security, init_net_stack.unix_namespace())
            .expect("Failed to mount pseudofs");
    }

    let loc = FS_CONTEXT
        .lock()
        .resolve(FsPath::new(args[0].as_bytes()))
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

    let boot_args: Vec<Vec<u8>> = args.iter().map(|arg| arg.as_bytes().to_vec()).collect();
    let boot_envs: Vec<Vec<u8>> = envs.iter().map(|env| env.as_bytes().to_vec()).collect();
    let boot_fs = FS_CONTEXT.lock().clone();
    let (entry_vaddr, ustack_top) =
        load_user_app_trusted(&mut uspace, &boot_fs, Some(&path), &boot_args, &boot_envs)
            .unwrap_or_else(|e| panic!("Failed to load user app: {}", e));

    let uctx = UserContext::new(entry_vaddr.into(), ustack_top, 0);
    let task_name = String::from_utf8_lossy(name.as_bytes()).into_owned();
    let mut task = try_new_user_task(task_name, uctx).expect("Failed to allocate init task");
    set_task_user_address_space(task.ctx_mut(), uspace.address_space_token());

    let tid = linux_pid_from_task_id(task.id().as_u64())
        .expect("init task identity must fit the Linux PID domain");
    let process_domain = init_process_domain().expect("Failed to allocate process domain");
    let prepared_zombie_snapshot =
        ProcessData::try_prepare_zombie_snapshot().expect("Failed to reserve init zombie snapshot");
    let init_pid_ns = PidNamespace::try_new_root_with_reaper_scope(
        user_ns.clone(),
        process_domain.root_reaper_scope(),
    )
    .expect("Failed to allocate init pid namespace");
    let init_pid_reservation = init_pid_ns
        .reserve_process(INIT_PID)
        .expect("Failed to reserve init pid namespace identity");
    let proc = process_domain
        .try_new_init_with_identity(INIT_PID, None, init_pid_ns.clone())
        .expect("Failed to allocate init process");

    // N_TTY owns only the physical input pump; init's controlling terminal
    // is the session-ownable VT1 endpoint exposed through /dev/console.
    let _ = &*N_TTY;
    VT_MANAGER
        .tty_for(1)
        .bind_to(&proc)
        .expect("Failed to bind VT1");

    let mut exe_path = Vec::new();
    exe_path
        .try_reserve_exact(path.as_bytes().len())
        .expect("Failed to allocate init executable path");
    exe_path.extend_from_slice(path.as_bytes());
    let exe_path = FsPathBuf::from_vec(exe_path);
    let mut cmdline = Vec::new();
    cmdline
        .try_reserve_exact(args.len())
        .expect("Failed to allocate init command line");
    for arg in args {
        let mut copy = Vec::new();
        copy.try_reserve_exact(arg.len())
            .expect("Failed to allocate init command-line argument");
        copy.extend_from_slice(arg.as_bytes());
        cmdline.push(copy);
    }
    let cmdline = Arc::try_new(cmdline).expect("Failed to allocate init command-line owner");
    let aspace = Arc::try_new(Mutex::new(uspace)).expect("Failed to allocate init address space");
    let access_state = ProcessAccessState::try_new(Dumpability::UserDumpable, user_ns.clone())
        .expect("Failed to allocate init process access state");
    let signal_actions = SharedSignalActions::try_new(SignalActions::default())
        .expect("Failed to allocate init signals");
    let init_fd_table =
        Arc::try_new(FdTable::new().expect("Failed to allocate init fd-table identity"))
            .expect("Failed to allocate init fd table");
    let init_fs_context = FS_CONTEXT.clone();
    let scope = try_new_process_scope().expect("Failed to allocate init process scope");
    let exit_fd_table =
        Arc::try_new(FdTable::new().expect("Failed to allocate init exit fd-table identity"))
            .expect("Failed to allocate init exit fd table");
    let init_uts_ns =
        UtsNamespace::try_new_root(user_ns.clone()).expect("Failed to allocate init UTS namespace");
    let init_mount_ns = MountNamespace::try_new_root(user_ns.clone())
        .expect("Failed to allocate init mount namespace");
    let init_ipc_ns = crate::syscall::ipc::IpcNamespace::try_new(user_ns.clone())
        .expect("Failed to allocate init IPC namespace");
    let init_cgroup_ns = CgroupNamespace::try_new_root(user_ns.clone())
        .expect("Failed to allocate init cgroup namespace");
    let init_time_ns = TimeNamespace::try_new_root(user_ns.clone())
        .expect("Failed to allocate init time namespace");
    let init_namespaces = NamespaceProxy::try_new(
        user_ns,
        init_pid_ns.clone(),
        init_mount_ns,
        init_ipc_ns,
        init_net_ns,
        init_cgroup_ns,
        init_uts_ns,
        init_time_ns,
    )
    .expect("Failed to assemble init namespace proxy");
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
        init_namespaces,
    )
    .expect("Failed to allocate init process runtime state");
    init_pid_reservation.commit();

    crate::file::add_stdio(&init_fd_table, &init_fs_context.lock()).expect("Failed to add stdio");

    let thread_admission = proc
        .prepare_thread(tid)
        .expect("Failed to admit init thread membership");
    let (thr, signal_registration) = Thread::try_new(
        tid,
        proc.clone(),
        credential,
        Arc::new(SeccompState::disabled()),
        FsContextSlot::new(init_fs_context.clone()),
        crate::task::FdTableSlot::new(init_fd_table),
        SchedulerSeed {
            state: SchedState::default(),
            reset_on_fork: false,
            uclamp: axtask::UclampRequest::unrestricted(),
            utilization_bounds: axtask::UtilizationBounds::unrestricted(),
            version: 0,
        },
    )
    .expect("Failed to allocate init thread state");
    proc.bind_initial_group_leader_signal(tid, thr.signal.clone(), thr.landlock_domain())
        .expect("Failed to bind init group-leader signal identity");
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
    signal_registration
        .commit()
        .expect("private init signal registration was cancelled before publication");
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

    let cx = init_fs_context.lock();
    cx.root_dir()
        .unmount_all()
        .expect("Failed to unmount all filesystems");
    cx.root_dir()
        .filesystem()
        .flush()
        .expect("Failed to flush rootfs");

    system_off();
}
