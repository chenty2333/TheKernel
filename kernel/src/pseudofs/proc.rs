use alloc::{
    borrow::Cow,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{any::Any, ffi::CStr, fmt::Write as _, iter, str, task::Context};

use axdriver::virtio_io_counters_snapshot;
use axerrno::{AxError, AxResult, LinuxError};
use axfs::render_io_stats_counters;
use axfs_ng_vfs::{
    DeviceId, DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps, FsName, FsNameBuf,
    FsPath, FsPathBuf, Location, Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission,
    NodeType, Reference, VfsError, VfsResult,
};
use axhal::paging::MappingFlags;
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use axtask::{AxTaskRef, WeakAxTaskRef, current};
use inherit_methods_macro::inherit_methods;
use linux_raw_sys::{
    general::{
        CAP_SYS_ADMIN, CAP_SYS_NICE, CLOCK_BOOTTIME, CLOCK_MONOTONIC, RLIM_INFINITY, RLIM_NLIMITS,
    },
    mempolicy::{
        MPOL_BIND, MPOL_DEFAULT, MPOL_INTERLEAVE, MPOL_LOCAL, MPOL_PREFERRED, MPOL_PREFERRED_MANY,
    },
};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};

#[cfg(feature = "bpf")]
use crate::bpf_security::{set_unprivileged_bpf_disabled, unprivileged_bpf_disabled};
#[cfg(feature = "test-io-control")]
use crate::file::io_uring::io_uring_dma_direct_stats_snapshot;
#[cfg(feature = "asid-switch-diagnostics")]
use crate::mm::asid_switch_diagnostics_snapshot;
#[cfg(feature = "mm-lock-diagnostics")]
use crate::mm::{MmLockStage, mm_lock_diagnostics_snapshot};
use crate::{
    file::{
        FileDescription, PidFd, current_file_operation_security_credential, fanotify::FanotifyFile,
        inotify::InotifyFile, lease, pipe, try_path_into_owned,
    },
    keyring::{
        KeyUserRecord, key_maxbytes, key_maxkeys, key_root_maxbytes, key_root_maxkeys,
        key_user_records, set_key_maxbytes, set_key_maxkeys, set_key_root_maxbytes,
        set_key_root_maxkeys,
    },
    mm::{
        AddrSpace, Backend, BackendOps, commit_limit_bytes, committed_as_bytes,
        memory_pressure_snapshot, memory_watermarks, overcommit_memory_policy, overcommit_ratio,
        set_overcommit_memory_policy, set_overcommit_ratio,
        system_memory_stats_with_reclaimable_file_cache, user_io_pin_counters_snapshot,
    },
    mounts,
    perf_security::{
        perf_event_max_sample_rate, perf_event_mlock_kb, perf_event_paranoid,
        set_perf_event_max_sample_rate, set_perf_event_mlock_kb, set_perf_event_paranoid,
    },
    pseudofs::{
        ChildNames, DirMaker, DirMapping, NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile,
        SimpleFileOperation, SimpleFileOps, SimpleFs, SimpleFsNode,
        cgroup::{
            proc_cgroup_membership, proc_cgroups_snapshot, proc_cpuset_membership,
            sched_util_clamp_max, sched_util_clamp_min, sched_util_clamp_min_rt_default,
            set_sched_util_clamp_max, set_sched_util_clamp_min,
            set_sched_util_clamp_min_rt_default,
        },
        try_boxed_names,
    },
    syscall::{
        aio_max_nr, aio_nr, current_can_administer_uts, current_domainname_bytes,
        current_hostname_bytes, current_machine_string, current_release_string,
        current_sysname_string, current_version_string, disable_kexec_load, ipc::IpcNamespace,
        kexec_load_disabled, mq_msg_max, mq_msgsize_max, mq_queues_max, msg_next_id, msgmni_limit,
        parse_sem_limits, proc_version_string, sched_rr_timeslice_ms, sem_limits_string,
        sem_next_id, set_aio_max_nr, set_domainname_bytes, set_hostname_bytes, set_mq_msg_max,
        set_mq_msgsize_max, set_mq_queues_max, set_msg_next_id, set_msgmni_limit,
        set_sched_rr_timeslice_ms, set_sem_limits, set_sem_next_id, set_shm_next_id,
        set_shmall_limit, set_shmmax_limit, set_shmmni_limit, shm_next_id, shmall_limit,
        shmmax_limit, shmmni_limit, sysvipc_msg_snapshot, sysvipc_sem_snapshot,
        sysvipc_shm_snapshot,
    },
    task::{
        AsThread, CgroupNamespace, Cred, ID_MAP_MAX_EXTENTS, IdMapInputExtent, Kgid, Kuid,
        Mempolicy, MempolicySnapshot, MountNamespace, NetworkNamespace, PidNamespace, Process,
        ProcessData, ProcessImageAccessSnapshot, PtraceAccessMode, TimeNamespace, UserNamespace,
        UtsNamespace, check_current_ptrace_image_snapshot,
        check_current_thread_ptrace_image_access, get_process_data, get_process_including_zombie,
        get_task, get_visible_task, get_visible_task_including_exiting, may_begin_gid_map_write,
        may_begin_uid_map_write, may_update_setgroups_policy, may_write_gid_map, may_write_uid_map,
        nr_open_limit, ns_capable, process_domain, process_error, render_task_stat,
        render_zombie_stat, set_nr_open_limit, task_state, validate_id_map_input,
    },
};

const PROC_SWAPS_HEADER: &str = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n";

fn validate_oom_score_adj_value(next_value: i32) -> VfsResult<()> {
    if !(-1000..=1000).contains(&next_value) {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

fn parse_oom_score_adj_value(data: &[u8]) -> VfsResult<i32> {
    let value = str::from_utf8(data)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .ok_or(VfsError::InvalidInput)?;
    validate_oom_score_adj_value(value)?;
    Ok(value)
}

fn timerslack_access_decision(same_task: bool, capable: bool) -> VfsResult<()> {
    if same_task || capable {
        Ok(())
    } else {
        Err(VfsError::PermissionDenied)
    }
}

fn check_timerslack_access(task: &AxTaskRef, actor: &Cred) -> VfsResult<()> {
    let same_task = Arc::ptr_eq(&current(), task);
    let target_cred = task.as_thread().current_cred();
    timerslack_access_decision(
        same_task,
        ns_capable(actor, target_cred.user_ns(), CAP_SYS_NICE),
    )
}

fn render_key_users_snapshot(
    records: Vec<KeyUserRecord>,
    viewer_user_ns: &UserNamespace,
) -> String {
    let mut out = String::new();
    for KeyUserRecord {
        uid,
        usage,
        keys,
        instantiated_keys,
        quota_keys,
        max_keys,
        quota_bytes,
        max_bytes,
    } in records
    {
        let Some(uid) = viewer_user_ns.kernel_uid_to_user(uid) else {
            continue;
        };
        let uid = uid.into_raw();
        let _ = writeln!(
            out,
            "{uid:5}: {usage:5} {keys}/{instantiated_keys} {quota_keys}/{max_keys} \
             {quota_bytes}/{max_bytes}"
        );
    }
    out
}

fn key_users_snapshot() -> VfsResult<String> {
    let viewer_user_ns = current().as_thread().current_user_namespace();
    let records = key_user_records().map_err(|error| match error {
        AxError::NoMemory => VfsError::NoMemory,
        _ => VfsError::Io,
    })?;
    Ok(render_key_users_snapshot(records, &viewer_user_ns))
}

fn try_pid_name(pid: u32) -> VfsResult<String> {
    let mut name = String::new();
    name.try_reserve_exact(10).map_err(|_| VfsError::NoMemory)?;
    write!(&mut name, "{pid}").map_err(|_| VfsError::Io)?;
    Ok(name)
}

fn render_proc_io_stats() -> Vec<u8> {
    let mut out = render_io_stats_counters();
    #[cfg(feature = "test-io-control")]
    {
        let io_uring_dma = io_uring_dma_direct_stats_snapshot();
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_hits {}",
            io_uring_dma.read_hits
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_bytes {}",
            io_uring_dma.read_bytes
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_fallbacks {}",
            io_uring_dma.read_fallbacks
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_fallback_geometry {}",
            io_uring_dma.read_fallback_geometry
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_fallback_provenance {}",
            io_uring_dma.read_fallback_provenance
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_fallback_sg_cap {}",
            io_uring_dma.read_fallback_sg_cap
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_fallback_extent {}",
            io_uring_dma.read_fallback_extent
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_read_fallback_device_admission {}",
            io_uring_dma.read_fallback_device_admission
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_hits {}",
            io_uring_dma.write_hits
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_bytes {}",
            io_uring_dma.write_bytes
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_fallbacks {}",
            io_uring_dma.write_fallbacks
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_fallback_geometry {}",
            io_uring_dma.write_fallback_geometry
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_fallback_provenance {}",
            io_uring_dma.write_fallback_provenance
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_fallback_sg_cap {}",
            io_uring_dma.write_fallback_sg_cap
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_fallback_extent {}",
            io_uring_dma.write_fallback_extent
        );
        let _ = writeln!(
            out,
            "io_uring.dma_direct_write_fallback_device_admission {}",
            io_uring_dma.write_fallback_device_admission
        );
        let _ = writeln!(
            out,
            "io_uring.physical_submitted {}",
            io_uring_dma.physical_submitted
        );
        let _ = writeln!(
            out,
            "io_uring.physical_child_submitted {}",
            io_uring_dma.physical_child_submitted
        );
        let _ = writeln!(
            out,
            "io_uring.physical_completed {}",
            io_uring_dma.physical_completed
        );
        let _ = writeln!(
            out,
            "io_uring.physical_child_completed {}",
            io_uring_dma.physical_child_completed
        );
        let _ = writeln!(
            out,
            "io_uring.physical_direct_bytes {}",
            io_uring_dma.physical_direct_bytes
        );
        let _ = writeln!(
            out,
            "io_uring.physical_qd_highwater {}",
            io_uring_dma.physical_qd_highwater
        );
        let _ = writeln!(
            out,
            "io_uring.physical_extent_highwater {}",
            io_uring_dma.physical_extent_highwater
        );
        let _ = writeln!(
            out,
            "io_uring.physical_quarantine {}",
            io_uring_dma.physical_quarantine
        );
    }
    let pin = user_io_pin_counters_snapshot();
    let _ = writeln!(out, "user_pin.to_user_attempts {}", pin.to_user_attempts);
    let _ = writeln!(out, "user_pin.to_user_hits {}", pin.to_user_hits);
    let _ = writeln!(out, "user_pin.to_user_bytes {}", pin.to_user_bytes);
    let _ = writeln!(
        out,
        "user_pin.from_user_attempts {}",
        pin.from_user_attempts
    );
    let _ = writeln!(out, "user_pin.from_user_hits {}", pin.from_user_hits);
    let _ = writeln!(out, "user_pin.from_user_bytes {}", pin.from_user_bytes);
    let _ = writeln!(out, "user_pin.reject_empty {}", pin.reject_empty);
    let _ = writeln!(out, "user_pin.reject_unaligned {}", pin.reject_unaligned);
    let _ = writeln!(out, "user_pin.reject_access {}", pin.reject_access);
    let _ = writeln!(out, "user_pin.reject_populate {}", pin.reject_populate);
    let _ = writeln!(out, "user_pin.reject_pagetable {}", pin.reject_pagetable);
    let _ = writeln!(out, "user_pin.reject_noncontig {}", pin.reject_noncontig);
    let _ = writeln!(out, "user_pin.reject_segments {}", pin.reject_segments);
    let _ = writeln!(out, "user_pin.reject_frame_pin {}", pin.reject_frame_pin);
    let _ = writeln!(
        out,
        "user_pin.reject_page_cache_pin {}",
        pin.reject_page_cache_pin
    );
    let _ = writeln!(out, "user_pin.reject_cow_pin {}", pin.reject_cow_pin);
    let _ = writeln!(out, "user_pin.reject_shared_pin {}", pin.reject_shared_pin);
    let _ = writeln!(out, "user_pin.reject_file_pin {}", pin.reject_file_pin);
    let _ = writeln!(out, "user_pin.reject_linear_pin {}", pin.reject_linear_pin);
    let _ = writeln!(out, "user_pin.sg_batches {}", pin.sg_batches);
    let _ = writeln!(out, "user_pin.sg_segments {}", pin.sg_segments);
    let _ = writeln!(out, "user_pin.sg_bytes {}", pin.sg_bytes);
    let _ = writeln!(
        out,
        "user_pin.sg_multi_segment_batches {}",
        pin.sg_multi_segment_batches
    );
    let _ = writeln!(out, "user_pin.direct_read_hits {}", pin.direct_read_hits);
    let _ = writeln!(out, "user_pin.direct_read_bytes {}", pin.direct_read_bytes);
    let _ = writeln!(
        out,
        "user_pin.direct_read_segments {}",
        pin.direct_read_segments
    );
    let _ = writeln!(
        out,
        "user_pin.direct_read_fallbacks {}",
        pin.direct_read_fallbacks
    );
    let _ = writeln!(out, "user_pin.direct_write_hits {}", pin.direct_write_hits);
    let _ = writeln!(
        out,
        "user_pin.direct_write_bytes {}",
        pin.direct_write_bytes
    );
    let _ = writeln!(
        out,
        "user_pin.direct_write_segments {}",
        pin.direct_write_segments
    );
    let _ = writeln!(
        out,
        "user_pin.direct_write_fallbacks {}",
        pin.direct_write_fallbacks
    );
    let _ = writeln!(
        out,
        "user_prefault.to_user_attempts {}",
        pin.prefault_to_user_attempts
    );
    let _ = writeln!(
        out,
        "user_prefault.to_user_hits {}",
        pin.prefault_to_user_hits
    );
    let _ = writeln!(
        out,
        "user_prefault.to_user_bytes {}",
        pin.prefault_to_user_bytes
    );
    let _ = writeln!(
        out,
        "user_prefault.to_user_rejects {}",
        pin.prefault_to_user_rejects
    );
    let _ = writeln!(
        out,
        "user_prefault.from_user_attempts {}",
        pin.prefault_from_user_attempts
    );
    let _ = writeln!(
        out,
        "user_prefault.from_user_hits {}",
        pin.prefault_from_user_hits
    );
    let _ = writeln!(
        out,
        "user_prefault.from_user_bytes {}",
        pin.prefault_from_user_bytes
    );
    let _ = writeln!(
        out,
        "user_prefault.from_user_rejects {}",
        pin.prefault_from_user_rejects
    );
    let _ = writeln!(out, "user_pin.cow_pin_pages {}", pin.cow_pin_pages);
    let _ = writeln!(out, "user_pin.shared_pin_pages {}", pin.shared_pin_pages);
    let _ = writeln!(out, "user_pin.file_pin_pages {}", pin.file_pin_pages);
    let _ = writeln!(
        out,
        "user_pin.frame_pin_attempts {}",
        pin.frame_pin_attempts
    );
    let _ = writeln!(out, "user_pin.frame_pin_hits {}", pin.frame_pin_hits);
    let _ = writeln!(out, "user_pin.frame_pin_pages {}", pin.frame_pin_pages);
    let _ = writeln!(out, "user_pin.frame_pin_bytes {}", pin.frame_pin_bytes);
    let _ = writeln!(out, "user_pin.frame_pin_unpins {}", pin.frame_pin_unpins);
    let _ = writeln!(
        out,
        "user_pin.page_cache_pin_attempts {}",
        pin.page_cache_pin_attempts
    );
    let _ = writeln!(
        out,
        "user_pin.page_cache_pin_hits {}",
        pin.page_cache_pin_hits
    );
    let _ = writeln!(
        out,
        "user_pin.page_cache_pin_pages {}",
        pin.page_cache_pin_pages
    );
    let _ = writeln!(
        out,
        "user_pin.page_cache_pin_bytes {}",
        pin.page_cache_pin_bytes
    );
    let _ = writeln!(
        out,
        "user_pin.page_cache_pin_unpins {}",
        pin.page_cache_pin_unpins
    );
    let _ = writeln!(
        out,
        "user_pin.vm_range_pin_attempts {}",
        pin.vm_range_pin_attempts
    );
    let _ = writeln!(out, "user_pin.vm_range_pin_hits {}", pin.vm_range_pin_hits);
    let _ = writeln!(
        out,
        "user_pin.vm_range_pin_bytes {}",
        pin.vm_range_pin_bytes
    );
    let _ = writeln!(
        out,
        "user_pin.vm_range_pin_rejects {}",
        pin.vm_range_pin_rejects
    );
    let _ = writeln!(
        out,
        "user_pin.vm_range_pin_unpins {}",
        pin.vm_range_pin_unpins
    );
    let _ = writeln!(out, "user_pin.unpins {}", pin.unpins);
    let virtio = virtio_io_counters_snapshot();
    let _ = writeln!(out, "virtio.queue_sync_waits {}", virtio.queue_sync_waits);
    let _ = writeln!(
        out,
        "virtio.queue_sync_wait_polls {}",
        virtio.queue_sync_wait_polls
    );
    let _ = writeln!(
        out,
        "virtio.queue_sync_wait_immediate {}",
        virtio.queue_sync_wait_immediate
    );
    let _ = writeln!(
        out,
        "virtio.queue_notify_calls {}",
        virtio.queue_notify_calls
    );
    let _ = writeln!(out, "virtio.blk_requests {}", virtio.blk_requests);
    let _ = writeln!(out, "virtio.blk_read_requests {}", virtio.blk_read_requests);
    let _ = writeln!(
        out,
        "virtio.blk_write_requests {}",
        virtio.blk_write_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_flush_requests {}",
        virtio.blk_flush_requests
    );
    let _ = writeln!(out, "virtio.blk_data_fences {}", virtio.blk_data_fences);
    let _ = writeln!(
        out,
        "virtio.blk_metadata_fences {}",
        virtio.blk_metadata_fences
    );
    let _ = writeln!(
        out,
        "virtio.blk_flush_unsupported {}",
        virtio.blk_flush_unsupported
    );
    let _ = writeln!(out, "virtio.blk_read_bytes {}", virtio.blk_read_bytes);
    let _ = writeln!(out, "virtio.blk_write_bytes {}", virtio.blk_write_bytes);
    let _ = writeln!(
        out,
        "virtio.blk_vectored_read_requests {}",
        virtio.blk_vectored_read_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_vectored_write_requests {}",
        virtio.blk_vectored_write_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_vectored_segments {}",
        virtio.blk_vectored_segments
    );
    let _ = writeln!(
        out,
        "virtio.blk_pending_max_depth {}",
        virtio.blk_pending_max_depth
    );
    let _ = writeln!(
        out,
        "virtio.blk_pending_queue_full {}",
        virtio.blk_pending_queue_full
    );
    let _ = writeln!(
        out,
        "virtio.blk_pending_drain_batches {}",
        virtio.blk_pending_drain_batches
    );
    let _ = writeln!(
        out,
        "virtio.blk_pending_drained_requests {}",
        virtio.blk_pending_drained_requests
    );
    let _ = writeln!(out, "virtio.blk_async_enabled {}", virtio.blk_async_enabled);
    let _ = writeln!(out, "virtio.blk_async_depth {}", virtio.blk_async_depth);
    let _ = writeln!(
        out,
        "virtio.blk_async_wait_policy {}",
        virtio.blk_async_wait_policy
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_adaptive_enabled {}",
        virtio.blk_async_adaptive_enabled
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_adaptive_depth {}",
        virtio.blk_async_adaptive_depth
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_adaptive_increases {}",
        virtio.blk_async_adaptive_increases
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_adaptive_decreases {}",
        virtio.blk_async_adaptive_decreases
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_adaptive_good_events {}",
        virtio.blk_async_adaptive_good_events
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_adaptive_pressure_events {}",
        virtio.blk_async_adaptive_pressure_events
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_merge_write_enabled {}",
        virtio.blk_async_merge_write_enabled
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_merge_write_calls {}",
        virtio.blk_async_merge_write_calls
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_merge_write_input_segments {}",
        virtio.blk_async_merge_write_input_segments
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_merge_write_output_requests {}",
        virtio.blk_async_merge_write_output_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_merge_write_saved_requests {}",
        virtio.blk_async_merge_write_saved_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_merge_write_max_segments {}",
        virtio.blk_async_merge_write_max_segments
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_flush_requests {}",
        virtio.blk_async_flush_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_flush_completions {}",
        virtio.blk_async_flush_completions
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_fallback_sync {}",
        virtio.blk_async_fallback_sync
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_submit_batches {}",
        virtio.blk_async_submit_batches
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_submit_requests {}",
        virtio.blk_async_submit_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_submit_bytes {}",
        virtio.blk_async_submit_bytes
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_submit_partial_batches {}",
        virtio.blk_async_submit_partial_batches
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_completion_batches {}",
        virtio.blk_async_completion_batches
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_completed_requests {}",
        virtio.blk_async_completed_requests
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_completed_bytes {}",
        virtio.blk_async_completed_bytes
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_max_depth {}",
        virtio.blk_async_max_depth
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_current_depth {}",
        virtio.blk_async_current_depth
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_desc_in_use_max {}",
        virtio.blk_async_desc_in_use_max
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_desc_budget {}",
        virtio.blk_async_desc_budget
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_admission_stalls {}",
        virtio.blk_async_admission_stalls
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_queue_full {}",
        virtio.blk_async_queue_full
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_notify_calls {}",
        virtio.blk_async_notify_calls
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_wait_spins {}",
        virtio.blk_async_wait_spins
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_wait_spin_hits {}",
        virtio.blk_async_wait_spin_hits
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_wait_yields {}",
        virtio.blk_async_wait_yields
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_wait_sleeps {}",
        virtio.blk_async_wait_sleeps
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_wait_wakeups {}",
        virtio.blk_async_wait_wakeups
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_wait_timeouts {}",
        virtio.blk_async_wait_timeouts
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_interrupt_drains {}",
        virtio.blk_async_interrupt_drains
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_arms {}",
        virtio.blk_async_irq_first_arms
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_waits {}",
        virtio.blk_async_irq_first_waits
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_fallbacks {}",
        virtio.blk_async_irq_first_fallbacks
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_fallback_unarmed {}",
        virtio.blk_async_irq_first_fallback_unarmed
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_fallback_cannot_block {}",
        virtio.blk_async_irq_first_fallback_cannot_block
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_fallback_no_irq {}",
        virtio.blk_async_irq_first_fallback_no_irq
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_fallback_register_failed {}",
        virtio.blk_async_irq_first_fallback_register_failed
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_irq_first_fallback_feature_disabled {}",
        virtio.blk_async_irq_first_fallback_feature_disabled
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_submit_errors {}",
        virtio.blk_async_submit_errors
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_completion_errors {}",
        virtio.blk_async_completion_errors
    );
    let _ = writeln!(
        out,
        "virtio.blk_async_resource_leaks {}",
        virtio.blk_async_resource_leaks
    );
    out.into_bytes()
}

/// Render the bounded, read-only cBPF execution counters used by the
/// performance evidence lane. The two adapters intentionally have separate
/// namespaces: a packet capture must never make a seccomp execution appear in
/// this file (or vice versa). The policy is diagnostic metadata only; it is
/// not a Linux ABI control surface.
fn render_proc_bpf_stats() -> Vec<u8> {
    let seccomp = crate::seccomp_jit::counters();
    let packet = crate::packet_cbpf::counters();
    let (seccomp_policy, packet_policy) = crate::seccomp_jit::executor_policies();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "BPF_STATS schema=thekernel-bpf-stats-v1 seccomp_policy={} packet_policy={}",
        seccomp_policy.as_str(),
        packet_policy.as_str(),
    );
    let _ = writeln!(out, "seccomp.published {}", seccomp.published);
    let _ = writeln!(out, "seccomp.native_executed {}", seccomp.native_executed);
    let _ = writeln!(
        out,
        "seccomp.interpreter_executed {}",
        seccomp.interpreter_executed
    );
    let _ = writeln!(
        out,
        "seccomp.fallback.policy_interpreter {}",
        seccomp.fallback_policy_interpreter
    );
    let _ = writeln!(
        out,
        "seccomp.fallback.translation {}",
        seccomp.fallback_translation
    );
    let _ = writeln!(
        out,
        "seccomp.fallback.publication {}",
        seccomp.fallback_publication
    );
    let _ = writeln!(out, "seccomp.fallback.owner {}", seccomp.fallback_owner);
    let _ = writeln!(
        out,
        "seccomp.fallback.unavailable {}",
        seccomp.fallback_unavailable
    );
    let _ = writeln!(out, "seccomp.jit_rejected {}", seccomp.jit_rejected);

    let _ = writeln!(out, "packet.published {}", packet.published);
    let _ = writeln!(out, "packet.native_executed {}", packet.native_executed);
    let _ = writeln!(
        out,
        "packet.interpreter_executed {}",
        packet.interpreter_executed
    );
    let _ = writeln!(
        out,
        "packet.fallback.policy_interpreter {}",
        packet.fallback_policy_interpreter
    );
    let _ = writeln!(
        out,
        "packet.fallback.translation {}",
        packet.fallback_translation
    );
    let _ = writeln!(
        out,
        "packet.fallback.publication {}",
        packet.fallback_publication
    );
    let _ = writeln!(out, "packet.fallback.owner {}", packet.fallback_owner);
    let _ = writeln!(
        out,
        "packet.fallback.unavailable {}",
        packet.fallback_unavailable
    );
    let _ = writeln!(out, "packet.jit_rejected {}", packet.jit_rejected);
    out.into_bytes()
}

/// One open instance of `/proc/bpf_stats`.
///
/// The proc directory is deliberately non-cacheable, so every path lookup
/// creates a fresh node.  The node then freezes both cBPF domains in its open
/// callback; subsequent `read_at` calls only consume that immutable byte
/// vector.  This keeps a short-buffer/chunked reader from combining counters
/// from multiple observations while leaving the ordinary 0444 proc
/// permission check in the VFS open path.
struct ProcBpfStatsFile {
    node: SimpleFsNode,
    snapshot: Mutex<Option<Vec<u8>>>,
}

impl ProcBpfStatsFile {
    fn try_new(fs: Arc<SimpleFs>) -> VfsResult<Arc<Self>> {
        let node = SimpleFsNode::try_new(
            fs,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o444),
        )?;
        Arc::try_new(Self {
            node,
            snapshot: Mutex::new(None),
        })
        .map_err(|_| VfsError::NoMemory)
    }

    fn snapshot_len(&self) -> u64 {
        self.snapshot
            .lock()
            .as_ref()
            .map_or(0, |data| data.len() as u64)
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for ProcBpfStatsFile {
    fn inode(&self) -> u64;

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.node.metadata()?;
        metadata.size = self.snapshot_len();
        Ok(metadata)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn filesystem(&self) -> &dyn FilesystemOps;

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(self.snapshot_len())
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn open(&self, _read: bool, write: bool) -> VfsResult<()> {
        if write {
            return Err(VfsError::PermissionDenied);
        }
        let data = render_proc_bpf_stats();
        let data_len = data.len() as u64;
        let mut snapshot = self.snapshot.lock();
        if snapshot.is_some() {
            return Ok(());
        }
        *snapshot = Some(data);
        drop(snapshot);
        self.node.metadata.lock().size = data_len;
        Ok(())
    }
}

impl FileNodeOps for ProcBpfStatsFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let snapshot = self.snapshot.lock();
        let data = snapshot.as_deref().ok_or(VfsError::BadFileDescriptor)?;
        let offset = usize::try_from(offset).map_err(|_| VfsError::InvalidInput)?;
        if offset >= data.len() {
            return Ok(0);
        }
        let data = &data[offset..];
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        Ok(read)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }

    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::BadFileDescriptor)
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }

    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

impl Pollable for ProcBpfStatsFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

#[cfg(feature = "test-io-control")]
fn render_proc_bpf_executor_control() -> Vec<u8> {
    let (seccomp, packet) = crate::seccomp_jit::executor_policies();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "BPF_EXECUTOR_CONTROL schema=thekernel-bpf-executor-control-v1"
    );
    let _ = writeln!(out, "seccomp={}", seccomp.as_str());
    let _ = writeln!(out, "packet={}", packet.as_str());
    out.into_bytes()
}

#[cfg(feature = "test-io-control")]
fn parse_bpf_executor_policy(value: &str) -> Option<crate::seccomp_jit::ExecutorPolicy> {
    match value {
        "auto" => Some(crate::seccomp_jit::ExecutorPolicy::Auto),
        "interpreter" => Some(crate::seccomp_jit::ExecutorPolicy::Interpreter),
        "jit" => Some(crate::seccomp_jit::ExecutorPolicy::Jit),
        _ => None,
    }
}

#[cfg(feature = "test-io-control")]
fn parse_bpf_executor_control(
    data: &[u8],
) -> VfsResult<(
    Option<crate::seccomp_jit::ExecutorPolicy>,
    Option<crate::seccomp_jit::ExecutorPolicy>,
)> {
    const MAX_CONTROL_BYTES: usize = 128;
    if data.is_empty() || data.len() > MAX_CONTROL_BYTES {
        return Err(VfsError::InvalidInput);
    }
    let text = str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let mut seccomp = None;
    let mut packet = None;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        // A single final newline is the only accepted empty line. Every
        // nonempty line is parsed before either policy is changed.
        if line.is_empty() {
            if lines.peek().is_none() && text.ends_with('\n') {
                break;
            }
            return Err(VfsError::InvalidInput);
        }
        if line.ends_with('\r') {
            return Err(VfsError::InvalidInput);
        }
        let (key, value) = line.split_once('=').ok_or(VfsError::InvalidInput)?;
        if key.is_empty() || value.is_empty() || key.trim() != key || value.trim() != value {
            return Err(VfsError::InvalidInput);
        }
        let policy = parse_bpf_executor_policy(value).ok_or(VfsError::InvalidInput)?;
        match key {
            "seccomp" if seccomp.is_none() => seccomp = Some(policy),
            "packet" if packet.is_none() => packet = Some(policy),
            _ => return Err(VfsError::InvalidInput),
        }
    }
    if seccomp.is_none() && packet.is_none() {
        return Err(VfsError::InvalidInput);
    }
    Ok((seccomp, packet))
}

#[cfg(feature = "test-io-control")]
fn bpf_executor_control_operation(request: SimpleFileOperation<'_>) -> VfsResult<Option<Vec<u8>>> {
    match request {
        SimpleFileOperation::Read => Ok(Some(render_proc_bpf_executor_control())),
        SimpleFileOperation::Write(data) => {
            // This is deliberately initial-root-only rather than a generic
            // capability gate: the node is a non-Linux test/perf control and
            // must not become a namespace-relative production interface.
            if !current().as_thread().current_cred().is_initial_root_euid() {
                return Err(VfsError::PermissionDenied);
            }
            let (seccomp, packet) = parse_bpf_executor_control(data)?;
            crate::seccomp_jit::set_executor_policies_for_control(seccomp, packet);
            Ok(None)
        }
    }
}

/// Render immutable scheduler-build configuration for low-perturbation
/// diagnosis.  This is a read-only snapshot of compile-time values; it does
/// not sample or mutate the EEVDF ready path.  Idle stealing is listed in a
/// separate record because it transfers queue ownership and is not an EEVDF
/// parameter or a scheduler-performance claim.
fn render_proc_sched_profile() -> Vec<u8> {
    let profile = axtask::eevdf_profile();
    let idle_steal = axtask::idle_steal_config();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "EEVDF_PROFILE schema=thekernel-eevdf-profile-v1 name={} normal_target_ticks={} \
         batch_target_ticks={} idle_target_ticks={} sleeper_grace_ticks={} sleeper_decay_ticks={}",
        profile.name,
        profile.normal_target_ticks,
        profile.batch_target_ticks,
        profile.idle_target_ticks,
        profile.sleeper_grace_ticks,
        profile.sleeper_decay_ticks,
    );
    let _ = writeln!(
        out,
        "IDLE_STEAL_CONFIG schema=thekernel-idle-steal-config-v1 enabled={} victim_scan_limit={} \
         candidate_scan_limit={} hot_residency_ns={} severe_imbalance_ready_tasks={}",
        u8::from(idle_steal.enabled),
        idle_steal.victim_scan_limit,
        idle_steal.candidate_scan_limit,
        idle_steal.hot_residency_ns,
        idle_steal.severe_imbalance_ready_tasks,
    );
    out.into_bytes()
}

#[cfg(feature = "mm-lock-diagnostics")]
fn render_proc_mm_lock_stats() -> Vec<u8> {
    let mut out = String::new();
    let first = mm_lock_diagnostics_snapshot(MmLockStage::ALL[0]);
    let _ = writeln!(
        out,
        "MM_LOCK_DIAGNOSTICS schema=thekernel-mm-lock-diagnostics-v1 enabled={} resetting={} \
         active_samples={} epoch={} sequence={} sequence_exhausted={} histogram=log2_ns_v1",
        u8::from(first.enabled),
        u8::from(first.resetting),
        first.active_samples,
        first.epoch,
        first.sequence,
        u8::from(first.sequence_exhausted)
    );

    for (index, stage) in MmLockStage::ALL.iter().copied().enumerate() {
        let snapshot = if index == 0 {
            first
        } else {
            mm_lock_diagnostics_snapshot(stage)
        };
        let stage = snapshot.stage;
        let _ = write!(
            out,
            "MM_LOCK_STAGE stage={} epoch={} samples={} wait_sum_ns={} wait_max_ns={} \
             hold_sum_ns={} hold_max_ns={} saturated={} wait_buckets=",
            stage.stage.as_str(),
            snapshot.epoch,
            stage.samples,
            stage.wait_ns,
            stage.max_wait_ns,
            stage.hold_ns,
            stage.max_hold_ns,
            u8::from(stage.saturated),
        );
        for (bucket, count) in stage.wait_buckets.iter().enumerate() {
            if bucket != 0 {
                out.push(',');
            }
            let _ = write!(out, "{count}");
        }
        out.push_str(" hold_buckets=");
        for (bucket, count) in stage.hold_buckets.iter().enumerate() {
            if bucket != 0 {
                out.push(',');
            }
            let _ = write!(out, "{count}");
        }
        out.push('\n');
    }
    let final_snapshot = mm_lock_diagnostics_snapshot(MmLockStage::ALL[0]);
    let _ = writeln!(
        out,
        "MM_LOCK_DIAGNOSTICS_END enabled={} resetting={} active_samples={} epoch={} sequence={} \
         sequence_exhausted={}",
        u8::from(final_snapshot.enabled),
        u8::from(final_snapshot.resetting),
        final_snapshot.active_samples,
        final_snapshot.epoch,
        final_snapshot.sequence,
        u8::from(final_snapshot.sequence_exhausted)
    );
    out.into_bytes()
}

#[cfg(feature = "asid-switch-diagnostics")]
fn render_proc_asid_switch_stats() -> Vec<u8> {
    let snapshot = asid_switch_diagnostics_snapshot();
    format!(
        "ASID_SWITCH_DIAGNOSTICS schema=thekernel-asid-switch-diagnostics-v1 enabled={} \
         fast_path_avoided={} fallback_asid_zero={} fallback_invalid_width={} \
         fallback_exhausted={} fallback_generation_mismatch={} fallback_same_id_different_root={} \
         saturated={}\n",
        u8::from(snapshot.enabled()),
        snapshot.fast_path_avoided(),
        snapshot.fallback_asid_zero(),
        snapshot.fallback_invalid_width(),
        snapshot.fallback_exhausted(),
        snapshot.fallback_generation_mismatch(),
        snapshot.fallback_same_id_different_root(),
        u8::from(snapshot.saturated()),
    )
    .into_bytes()
}

#[cfg(feature = "pmu-diagnostics")]
fn render_proc_pmu_capabilities() -> Vec<u8> {
    let snapshot = crate::pmu::capability_snapshot();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "PMU_CAPABILITIES schema=thekernel-pmu-capabilities-v1 source={} counter_count={} \
         consistent_snapshot={} samples_collected=0",
        snapshot.source(),
        snapshot.counter_count(),
        u8::from(snapshot.has_consistent_snapshot()),
    );
    for (event, requestable) in snapshot.events() {
        let _ = writeln!(
            out,
            "PMU_EVENT event={event} requestable={} sampled=0",
            u8::from(requestable),
        );
    }
    out.into_bytes()
}

const PROC_PAGEMAP_ENTRY_BYTES: u64 = 8;
const PROC_NUMA_NODEMASK: usize = 0b1;
const PROC_LIMIT_NAMES: [(&str, Option<&str>); RLIM_NLIMITS as usize] = [
    ("Max cpu time", Some("seconds")),
    ("Max file size", Some("bytes")),
    ("Max data size", Some("bytes")),
    ("Max stack size", Some("bytes")),
    ("Max core file size", Some("bytes")),
    ("Max resident set", Some("bytes")),
    ("Max processes", Some("processes")),
    ("Max open files", Some("files")),
    ("Max locked memory", Some("bytes")),
    ("Max address space", Some("bytes")),
    ("Max file locks", Some("locks")),
    ("Max pending signals", Some("signals")),
    ("Max msgqueue size", Some("bytes")),
    ("Max nice priority", None),
    ("Max realtime priority", None),
    ("Max realtime timeout", Some("us")),
];
fn append_mount_data_options(options: &mut String, data: &str) {
    for option in data
        .split(',')
        .map(|option| option.trim())
        .filter(|option| !option.is_empty())
    {
        if !options.split(',').any(|existing| existing == option) {
            options.push(',');
            options.push_str(option);
        }
    }
}

fn record_mount_options(record: &mounts::MountRecord) -> String {
    let mut options = mounts::mount_options(record.flags);
    let data = match record.fs_type.as_str() {
        "cgroup" if !record.data.is_empty() => Some(record.data.as_str()),
        "cgroup" if !matches!(record.source.as_bytes(), b"none" | b"cgroup") => {
            core::str::from_utf8(record.source.as_bytes()).ok()
        }
        "cgroup2" if !record.data.is_empty() => Some(record.data.as_str()),
        _ => None,
    };
    if let Some(data) = data {
        append_mount_data_options(&mut options, data);
    }
    options
}

fn escape_mount_field(field: &[u8], escaped: &mut Vec<u8>) -> VfsResult<()> {
    escaped
        .try_reserve(field.len())
        .map_err(|_| VfsError::NoMemory)?;
    for byte in field {
        match byte {
            b' ' => escaped.extend_from_slice(b"\\040"),
            b'\t' => escaped.extend_from_slice(b"\\011"),
            b'\n' => escaped.extend_from_slice(b"\\012"),
            b'\\' => escaped.extend_from_slice(b"\\134"),
            byte => escaped.push(*byte),
        }
    }
    Ok(())
}

fn render_mounts() -> VfsResult<Vec<u8>> {
    let mut out = Vec::new();
    for record in mounts::snapshot()? {
        let options = record_mount_options(&record);
        escape_mount_field(record.source.as_bytes(), &mut out)?;
        out.push(b' ');
        escape_mount_field(record.target.as_bytes(), &mut out)?;
        out.extend_from_slice(alloc::format!(" {} {} 0 0\n", record.fs_type, options).as_bytes());
    }
    Ok(out)
}

fn render_mountinfo() -> VfsResult<Vec<u8>> {
    let mut out = Vec::new();
    for record in mounts::snapshot()? {
        let dev = DeviceId(record.dev);
        let options = record_mount_options(&record);
        out.extend_from_slice(
            alloc::format!(
                "{} {} {}:{} ",
                record.mount_id,
                record.parent_id,
                dev.major(),
                dev.minor()
            )
            .as_bytes(),
        );
        escape_mount_field(record.root.as_bytes(), &mut out)?;
        out.push(b' ');
        escape_mount_field(record.target.as_bytes(), &mut out)?;
        out.extend_from_slice(alloc::format!(" {} - {} ", options, record.fs_type).as_bytes());
        escape_mount_field(record.source.as_bytes(), &mut out)?;
        out.extend_from_slice(alloc::format!(" {options}\n").as_bytes());
    }
    Ok(out)
}

fn proc_task_for_pid(pid: u32) -> VfsResult<AxTaskRef> {
    if let Ok(task) = get_visible_task_including_exiting(pid) {
        return Ok(task);
    }

    let proc_data = get_process_data(pid).map_err(|_| VfsError::NotFound)?;
    for tid in proc_data.proc.thread_ids() {
        if let Ok(task) = get_task(tid)
            && !task.as_thread().pending_exit()
        {
            return Ok(task);
        }
    }

    Err(VfsError::NotFound)
}

fn proc_subject_cred(task: &AxTaskRef, process_view: bool) -> Arc<Cred> {
    let thread = task.as_thread();
    if process_view {
        thread.proc_data.group_leader_cred()
    } else {
        thread.current_cred()
    }
}

fn proc_image_access(
    task: &AxTaskRef,
    process_view: bool,
) -> VfsResult<ProcessImageAccessSnapshot> {
    let target = task.as_thread();
    if process_view {
        let snapshot = target.proc_data.group_leader_image_access_snapshot();
        check_current_ptrace_image_snapshot(&target.proc_data, &snapshot, PtraceAccessMode::ReadFs)
            .map_err(|_| VfsError::PermissionDenied)?;
        Ok(snapshot)
    } else {
        check_current_thread_ptrace_image_access(target, PtraceAccessMode::ReadFs)
            .map_err(|_| VfsError::PermissionDenied)
    }
}

fn proc_fd_image_access(task: &AxTaskRef, process_view: bool) -> VfsResult<Arc<Mutex<AddrSpace>>> {
    let proc_data = &task.as_thread().proc_data;
    if proc_data.exec_in_progress() {
        return Err(VfsError::PermissionDenied);
    }
    Ok(proc_image_access(task, process_view)?.into_aspace())
}

fn validate_proc_fd_image(
    task: &AxTaskRef,
    authorized_image: &Arc<Mutex<AddrSpace>>,
) -> VfsResult<()> {
    let proc_data = &task.as_thread().proc_data;
    if proc_data.exec_in_progress() || !proc_data.image_matches(authorized_image) {
        Err(VfsError::PermissionDenied)
    } else {
        Ok(())
    }
}

fn parse_id_map_rows(data: &[u8]) -> VfsResult<Vec<IdMapInputExtent>> {
    if data.is_empty() || data.len() >= PAGE_SIZE_4K {
        return Err(VfsError::InvalidInput);
    }
    let text = str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let line_count = text.lines().count();
    if line_count == 0 || line_count > ID_MAP_MAX_EXTENTS {
        return Err(VfsError::InvalidInput);
    }
    let mut rows = Vec::new();
    rows.try_reserve_exact(line_count)
        .map_err(|_| VfsError::NoMemory)?;
    for line in text.lines() {
        if line.trim().is_empty() {
            return Err(VfsError::InvalidInput);
        }
        let mut fields = line.split_ascii_whitespace();
        let first = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or(VfsError::InvalidInput)?;
        let lower_first = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or(VfsError::InvalidInput)?;
        let count = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or(VfsError::InvalidInput)?;
        if fields.next().is_some() {
            return Err(VfsError::InvalidInput);
        }
        rows.push(IdMapInputExtent::new(first, lower_first, count));
    }
    Ok(rows)
}

fn parse_setgroups_policy(data: &[u8]) -> VfsResult<bool> {
    if data.len() >= 8 {
        return Err(VfsError::InvalidInput);
    }
    let text = str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let (allow, trailing) = if let Some(trailing) = text.strip_prefix("allow") {
        (true, trailing)
    } else if let Some(trailing) = text.strip_prefix("deny") {
        (false, trailing)
    } else {
        return Err(VfsError::InvalidInput);
    };
    if !trailing.bytes().all(|byte| byte.is_ascii_whitespace()) {
        return Err(VfsError::InvalidInput);
    }
    Ok(allow)
}

fn require_proc_userns_write_offset(offset: u64) -> VfsResult<()> {
    if offset == 0 {
        Ok(())
    } else {
        Err(VfsError::InvalidInput)
    }
}

fn render_id_map(
    namespace: &Arc<UserNamespace>,
    viewer: &Arc<UserNamespace>,
    uid: bool,
) -> VfsResult<Vec<u8>> {
    let rows = if uid {
        namespace.try_uid_map_rows(viewer)?
    } else {
        namespace.try_gid_map_rows(viewer)?
    };
    let capacity = rows.len().checked_mul(33).ok_or(VfsError::NoMemory)?;
    let mut output = String::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| VfsError::NoMemory)?;
    for row in rows {
        writeln!(
            output,
            "{:>10} {:>10} {:>10}",
            row.first, row.lower_first, row.count
        )
        .map_err(|_| VfsError::Io)?;
    }
    Ok(output.into_bytes())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ProcUserNamespaceFileKind {
    UidMap,
    GidMap,
    Setgroups,
}

/// Offset-aware proc user-namespace control node.
///
/// These files cannot use `SimpleFile`: Linux requires writes to start at
/// offset zero and freezes the opener credential for map authorization.
struct ProcUserNamespaceFile {
    node: SimpleFsNode,
    namespace: Arc<UserNamespace>,
    kind: ProcUserNamespaceFileKind,
}

impl ProcUserNamespaceFile {
    fn try_new(
        fs: Arc<SimpleFs>,
        namespace: Arc<UserNamespace>,
        kind: ProcUserNamespaceFileKind,
        owner_uid: Kuid,
        owner_gid: Kgid,
    ) -> VfsResult<Arc<Self>> {
        let node = SimpleFsNode::try_new(
            fs,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o644),
        )?;
        {
            let mut metadata = node.metadata.lock();
            metadata.uid = owner_uid.into_raw();
            metadata.gid = owner_gid.into_raw();
        }
        Arc::try_new(Self {
            node,
            namespace,
            kind,
        })
        .map_err(|_| VfsError::NoMemory)
    }

    fn try_render(&self) -> VfsResult<Vec<u8>> {
        match self.kind {
            ProcUserNamespaceFileKind::UidMap | ProcUserNamespaceFileKind::GidMap => {
                let viewer = current_file_operation_security_credential().ok_or(VfsError::Io)?;
                render_id_map(
                    &self.namespace,
                    viewer.user_ns(),
                    self.kind == ProcUserNamespaceFileKind::UidMap,
                )
            }
            ProcUserNamespaceFileKind::Setgroups => {
                let value: &[u8] = if self.namespace.setgroups_allowed() {
                    b"allow\n"
                } else {
                    b"deny\n"
                };
                let mut output = Vec::new();
                output
                    .try_reserve_exact(value.len())
                    .map_err(|_| VfsError::NoMemory)?;
                output.extend_from_slice(value);
                Ok(output)
            }
        }
    }

    fn write_from_zero(&self, data: &[u8]) -> VfsResult<()> {
        let opener = current_file_operation_security_credential().ok_or(VfsError::Io)?;
        match self.kind {
            ProcUserNamespaceFileKind::UidMap => {
                if !may_begin_uid_map_write(&opener, &self.namespace) {
                    return Err(VfsError::OperationNotPermitted);
                }
                let rows = parse_id_map_rows(data)?;
                validate_id_map_input(&rows).map_err(crate::task::cred_error)?;
                let actor = current().as_thread().current_cred();
                if !may_write_uid_map(&actor, &opener, &self.namespace, &rows) {
                    return Err(VfsError::OperationNotPermitted);
                }
                let map = self.namespace.try_build_uid_map_from_slice(&rows)?;
                self.namespace.publish_uid_map(map)?;
            }
            ProcUserNamespaceFileKind::GidMap => {
                if !may_begin_gid_map_write(&opener, &self.namespace) {
                    return Err(VfsError::OperationNotPermitted);
                }
                let rows = parse_id_map_rows(data)?;
                validate_id_map_input(&rows).map_err(crate::task::cred_error)?;
                let actor = current().as_thread().current_cred();
                if !may_write_gid_map(&actor, &opener, &self.namespace, &rows) {
                    return Err(VfsError::OperationNotPermitted);
                }
                let map = self.namespace.try_build_gid_map_from_slice(&rows)?;
                // If unprivileged authorization succeeded, setgroups was
                // already irreversibly denied. Publication serializes the
                // one-shot map state with that policy.
                self.namespace.publish_gid_map(map, false)?;
            }
            ProcUserNamespaceFileKind::Setgroups => {
                if !may_update_setgroups_policy(&opener, &self.namespace) {
                    return Err(VfsError::OperationNotPermitted);
                }
                let allow = parse_setgroups_policy(data)?;
                self.namespace.update_setgroups_policy(allow)?;
            }
        }
        Ok(())
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for ProcUserNamespaceFile {
    fn inode(&self) -> u64;

    fn metadata(&self) -> VfsResult<Metadata>;

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn filesystem(&self) -> &dyn FilesystemOps;

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
            | NodeFlags::POSITIONED_APPEND
            | NodeFlags::OPEN_CREDENTIAL
            | NodeFlags::NO_POSITIONED_WRITE
    }

    fn open(&self, _read: bool, write: bool) -> VfsResult<()> {
        let opener = current().as_thread().current_cred();
        if write
            && self.kind == ProcUserNamespaceFileKind::Setgroups
            && !may_update_setgroups_policy(&opener, &self.namespace)
        {
            // Linux rejects the writable open itself with EACCES.
            return Err(VfsError::PermissionDenied);
        }
        Ok(())
    }
}

impl FileNodeOps for ProcUserNamespaceFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.try_render()?;
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let data = &data[offset as usize..];
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        Ok(read)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        require_proc_userns_write_offset(offset)?;
        self.write_from_zero(buf)?;
        Ok(buf.len())
    }

    fn write_at_vectored(&self, bufs: &[&[u8]], offset: u64) -> VfsResult<usize> {
        require_proc_userns_write_offset(offset)?;
        let len = bufs.iter().try_fold(0usize, |total, buf| {
            total.checked_add(buf.len()).ok_or(VfsError::InvalidInput)
        })?;
        if len == 0 {
            return Ok(0);
        }
        // map_write() accepts less than one page. Reject oversized vectors
        // before allocating a contiguous transaction buffer.
        if len >= PAGE_SIZE_4K {
            return Err(VfsError::InvalidInput);
        }
        let mut data = Vec::new();
        data.try_reserve_exact(len)
            .map_err(|_| VfsError::NoMemory)?;
        for buf in bufs {
            data.extend_from_slice(buf);
        }
        self.write_from_zero(&data)?;
        Ok(len)
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let written = self.write_at(buf, 0)?;
        Ok((written, written as u64))
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        // Linux accepts O_TRUNC and ftruncate on these proc controls without
        // changing their generated contents or one-shot publication state.
        Ok(())
    }

    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

impl Pollable for ProcUserNamespaceFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

fn real_meminfo() -> String {
    let reclaim = axfs::cached_file_reclaim_estimate();
    let stats = system_memory_stats_with_reclaimable_file_cache(reclaim.reclaimable_pages);
    let total_kb = stats.total_bytes / 1024;
    let free_kb = stats.free_bytes / 1024;
    let available_kb = stats.available_bytes / 1024;
    let cached_kb = stats.cached_bytes / 1024;
    let page_tables_kb = stats.page_table_bytes / 1024;
    let commit_limit_kb = commit_limit_bytes() / 1024;
    let committed_kb = committed_as_bytes() / 1024;
    format!(
        "MemTotal:       {total_kb:>8} kB\nMemFree:        {free_kb:>8} kB\nMemAvailable:   \
         {available_kb:>8} kB\nBuffers:               0 kB\nCached:         {cached_kb:>8} \
         kB\nSwapCached:            0 kB\nSwapTotal:             0 kB\nSwapFree:              0 \
         kB\nPageTables:     {page_tables_kb:>8} kB\nCommitLimit:    {commit_limit_kb:>8} \
         kB\nCommitted_AS:   {committed_kb:>8} kB\n"
    )
}

fn real_memory_pressure() -> String {
    let snapshot = memory_pressure_snapshot();
    let reclaim = axfs::cached_file_reclaim_estimate();
    let memory = system_memory_stats_with_reclaimable_file_cache(reclaim.reclaimable_pages);
    let total_pages = memory.total_bytes / PAGE_SIZE_4K;
    let free_pages = memory.free_bytes / PAGE_SIZE_4K;
    let watermarks = memory_watermarks(total_pages);
    format!(
        concat!(
            "schema=thekernel-mm-pressure-v1\n",
            "total_pages={}\n",
            "free_pages={}\n",
            "low_watermark_pages={}\n",
            "high_watermark_pages={}\n",
            "mem_available_bytes={}\n",
            "low_watermark_bytes={}\n",
            "reclaimable_clean_file_bytes={}\n",
            "reclaimable_clean_file_pages={}\n",
            "reclaim_estimate_scanned_files={}\n",
            "reclaim_estimate_busy_files={}\n",
            "reclaim_estimate_mapped_files={}\n",
            "reclaim_estimate_truncated={}\n",
            "checks={}\n",
            "pressure_events={}\n",
            "reclaim_passes={}\n",
            "reclaimed_pages={}\n",
            "no_progress_passes={}\n",
            "visited_registry_entries={}\n",
            "scanned_files={}\n",
            "scanned_pages={}\n",
            "dirty_pages={}\n",
            "pinned_pages={}\n",
            "writeback_pages={}\n",
            "busy_files={}\n",
            "mapped_files={}\n",
            "scan_budget_exhausted_files={}\n",
            "snapshot_truncations={}\n",
        ),
        total_pages,
        free_pages,
        watermarks.low_pages,
        watermarks.high_pages,
        memory.available_bytes,
        memory.low_watermark_bytes,
        memory.reclaimable_file_cache_bytes,
        reclaim.reclaimable_pages,
        reclaim.scanned_files,
        reclaim.busy_files,
        reclaim.mapped_files,
        usize::from(reclaim.snapshot_truncated),
        snapshot.checks,
        snapshot.pressure_events,
        snapshot.reclaim_passes,
        snapshot.reclaimed_pages,
        snapshot.no_progress_passes,
        snapshot.visited_registry_entries,
        snapshot.scanned_files,
        snapshot.scanned_pages,
        snapshot.dirty_pages,
        snapshot.pinned_pages,
        snapshot.writeback_pages,
        snapshot.busy_files,
        snapshot.mapped_files,
        snapshot.scan_budget_exhausted_files,
        snapshot.snapshot_truncations,
    )
}

fn current_net_ipv4_conf_tag(iface: &str) -> VfsResult<i32> {
    current()
        .as_thread()
        .net_ns()
        .stack()
        .ipv4_conf_tag(iface)
        .ok_or(VfsError::NotFound)
}

fn set_current_net_ipv4_conf_tag(iface: &str, value: i32) -> VfsResult<()> {
    current()
        .as_thread()
        .net_ns()
        .stack()
        .set_ipv4_conf_tag(iface, value)
        .map_err(|_| VfsError::NotFound)
}

fn proc_ipv4_conf_tag_file(iface: &'static str) -> impl crate::pseudofs::SimpleFileOps {
    RwFile::new_root_writable(move |req| match req {
        SimpleFileOperation::Read => Ok(Some(
            format!("{}\n", current_net_ipv4_conf_tag(iface)?).into_bytes(),
        )),
        SimpleFileOperation::Write(data) => {
            if data.iter().all(|byte| byte.is_ascii_whitespace()) {
                return Ok(None);
            }
            let current = current();
            let thread = current.as_thread();
            let cred = thread.current_cred();
            let net_ns = thread.net_ns();
            if !ns_capable(
                &cred,
                net_ns.owner_user_ns(),
                linux_raw_sys::general::CAP_NET_ADMIN,
            ) {
                return Err(VfsError::PermissionDenied);
            }
            let value = str::from_utf8(data)
                .ok()
                .map(str::trim)
                .and_then(|it| it.parse::<i32>().ok())
                .ok_or(VfsError::InvalidInput)?;
            set_current_net_ipv4_conf_tag(iface, value)?;
            Ok(None)
        }
    })
}

fn is_shared_user_mapping(backend: &Backend) -> bool {
    matches!(
        backend,
        Backend::Shared(_) | Backend::File(_) | Backend::Linear(_)
    )
}

pub fn new_procfs() -> Filesystem {
    SimpleFs::new_with("proc".into(), 0x9fa0, builder)
}

struct ProcessTaskDir {
    fs: Arc<SimpleFs>,
    process: Weak<Process>,
}

impl SimpleDirOps for ProcessTaskDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let Some(process) = self.process.upgrade() else {
            return try_boxed_names(iter::empty());
        };
        let mut names = Vec::new();
        names
            .try_reserve_exact(process.thread_count())
            .map_err(|_| VfsError::NoMemory)?;
        for tid in process.thread_ids() {
            let Ok(task) = get_task(tid) else {
                continue;
            };
            if task.as_thread().pending_exit() {
                continue;
            }
            names.push(Cow::Owned(FsNameBuf::from_vec(
                try_pid_name(task.as_thread().tid())?.into_bytes(),
            )?));
        }
        try_boxed_names(names.into_iter())
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let process = self.process.upgrade().ok_or(VfsError::NotFound)?;
        let tid = str::from_utf8(name.as_bytes())
            .map_err(|_| VfsError::NotFound)?
            .parse::<u32>()
            .map_err(|_| VfsError::NotFound)?;
        let task = get_visible_task(tid).map_err(|_| VfsError::NotFound)?;
        if task.as_thread().proc_data.proc.pid() != process.pid() {
            return Err(VfsError::NotFound);
        }

        Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(ThreadDir {
                fs: self.fs.clone(),
                task: Arc::downgrade(&task),
                show_task_dir: false,
            }),
        )))
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

fn format_cap_set(words: [u32; 2]) -> String {
    format!("{:016x}", ((words[1] as u64) << 32) | words[0] as u64)
}

fn task_cpu_mask_bits(task: &AxTaskRef) -> usize {
    let cpus = axhal::cpu_num().max(1).min(usize::BITS as usize);
    let cpumask = task.cpumask();
    let mut mask = 0usize;
    for cpu in 0..cpus {
        if cpumask.get(cpu) {
            mask |= 1usize << cpu;
        }
    }
    if mask != 0 { mask } else { 1 }
}

fn format_mask_list(mask: usize, width: usize) -> String {
    let mut ranges = Vec::new();
    let mut index = 0usize;
    let width = width.min(usize::BITS as usize);
    while index < width {
        if mask & (1usize << index) == 0 {
            index += 1;
            continue;
        }
        let start = index;
        while index + 1 < width && mask & (1usize << (index + 1)) != 0 {
            index += 1;
        }
        if start == index {
            ranges.push(start.to_string());
        } else {
            ranges.push(format!("{start}-{index}"));
        }
        index += 1;
    }
    if ranges.is_empty() {
        "0".into()
    } else {
        ranges.join(",")
    }
}

#[rustfmt::skip]
fn task_status(
    task: &AxTaskRef,
    process_view: bool,
    viewer_user_ns: &UserNamespace,
) -> VfsResult<String> {
    let thread = task.as_thread();
    let proc_data = &thread.proc_data;
    let (vm_size_kb, vm_rss_kb, locked_kb) = {
        let aspace_handle = proc_data.aspace();
        let aspace = aspace_handle.lock();
        let vm_size = aspace
            .areas()
            .filter(|area| area.flags().contains(MappingFlags::USER))
            .map(|area| area.size())
            .sum::<usize>();
        (
            vm_size / 1024,
            aspace.resident_user_bytes() / 1024,
            aspace.locked_bytes() / 1024,
        )
    };
    let state = task_state(task);
    let state_name = match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "disk sleep",
        'T' => "stopped",
        'Z' => "zombie",
        _ => "unknown",
    };
    let ppid = proc_data.proc.parent().map_or(0, |parent| parent.pid());
    let threads = proc_data.proc.thread_count();
    let cred = proc_subject_cred(task, process_view);
    let ids = cred.ids();
    let caps = cred.capabilities();
    let seccomp = thread.seccomp_snapshot();
    let seccomp_filter_count = seccomp.filter_count();
    let cpu_mask = task_cpu_mask_bits(task);
    let mem_mask = PROC_NUMA_NODEMASK;
    let cpu_width = axhal::cpu_num().max(1);
    let mem_width = PROC_NUMA_NODEMASK
        .next_power_of_two()
        .trailing_zeros()
        .max(1) as usize;
    let cpu_allowed_list = format_mask_list(cpu_mask, cpu_width);
    let mem_allowed_list = format_mask_list(mem_mask, mem_width);
    let task_name = task.try_name().map_err(|_| VfsError::NoMemory)?;
    #[cfg(target_arch = "x86_64")]
    let (cet_features_bits, cet_locked_bits) = if Arc::ptr_eq(task, &current()) {
        let state = crate::task::current_user_live_cet_state();
        (state.u_cet, state.locked)
    } else {
        // This is a task-published policy snapshot, never a foreign CPU's
        // MSR image. It remains available while the target is running, ready,
        // blocked, or exiting, unlike ptrace's stopped-context interface.
        axtask::snapshot_task_user_cet_status(task)
    };
    #[cfg(target_arch = "x86_64")]
    let cet_names = |bits: u64| {
        let mut names = Vec::new();
        if bits & 1 != 0 { names.push("shstk"); }
        if bits & 2 != 0 { names.push("wrss"); }
        names.join(" ")
    };
    #[cfg(target_arch = "x86_64")]
    let cet_features = cet_names(cet_features_bits);
    #[cfg(target_arch = "x86_64")]
    let cet_locked = cet_names(cet_locked_bits);
    Ok(format!(
        "Name:\t{}\n\
        State:\t{} ({})\n\
        Tgid:\t{}\n\
        Pid:\t{}\n\
        PPid:\t{}\n\
        Uid:\t{} {} {} {}\n\
        Gid:\t{} {} {} {}\n\
        VmSize:\t{} kB\n\
        VmRSS:\t{} kB\n\
        VmLck:\t{} kB\n\
        VmSwap:\t0 kB\n\
        Threads:\t{}\n\
        NoNewPrivs:\t{}\n\
        Seccomp:\t{}\n\
        Seccomp_filters:\t{}\n\
        CapInh:\t{}\n\
        CapPrm:\t{}\n\
        CapEff:\t{}\n\
        CapBnd:\t{}\n\
        CapAmb:\t{}\n\
        Cpus_allowed:\t{:x}\n\
        Cpus_allowed_list:\t{}\n\
        x86_Thread_features:\t{}\n\
        x86_Thread_features_locked:\t{}\n\
        Mems_allowed:\t{:x}\n\
        Mems_allowed_list:\t{}",
        task_name,
        state,
        state_name,
        proc_data.proc.pid(),
        if process_view { proc_data.proc.pid() } else { thread.tid() },
        ppid,
        viewer_user_ns.from_kuid_munged(ids.ruid),
        viewer_user_ns.from_kuid_munged(ids.euid),
        viewer_user_ns.from_kuid_munged(ids.suid),
        viewer_user_ns.from_kuid_munged(ids.fsuid),
        viewer_user_ns.from_kgid_munged(ids.rgid),
        viewer_user_ns.from_kgid_munged(ids.egid),
        viewer_user_ns.from_kgid_munged(ids.sgid),
        viewer_user_ns.from_kgid_munged(ids.fsgid),
        vm_size_kb,
        vm_rss_kb,
        locked_kb,
        threads,
        cred.no_new_privs() as u8,
        seccomp.mode() as u8,
        seccomp_filter_count,
        format_cap_set(caps.inheritable()),
        format_cap_set(caps.permitted()),
        format_cap_set(caps.effective()),
        format_cap_set(caps.bounding()),
        format_cap_set(caps.ambient()),
        cpu_mask,
        cpu_allowed_list,
        cet_features,
        cet_locked,
        mem_mask,
        mem_allowed_list
    ))
}

fn format_rlimit_value(value: u64) -> String {
    if value == RLIM_INFINITY as i64 as u64 {
        "unlimited".into()
    } else {
        value.to_string()
    }
}

fn render_task_limits(task: &AxTaskRef) -> Vec<u8> {
    let limits = task.as_thread().proc_data.rlim.read();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<25} {:<20} {:<20} {:<10}",
        "Limit", "Soft Limit", "Hard Limit", "Units"
    );

    for (resource, (name, unit)) in PROC_LIMIT_NAMES.iter().enumerate() {
        let limit = &limits[resource as u32];
        let soft = format_rlimit_value(limit.current);
        let hard = format_rlimit_value(limit.max);
        if let Some(unit) = unit {
            let _ = writeln!(out, "{name:<25} {soft:<20} {hard:<20} {unit:<10}");
        } else {
            let _ = writeln!(out, "{name:<25} {soft:<20} {hard:<20}");
        }
    }

    out.into_bytes()
}

fn render_task_maps(aspace_handle: &Arc<Mutex<AddrSpace>>, include_smaps: bool) -> String {
    let aspace = aspace_handle.lock();
    let mut out = String::new();

    for area in aspace.areas() {
        if !area.flags().contains(MappingFlags::USER) {
            continue;
        }
        let start = area.start().as_usize();
        let end = start + area.size();
        let flags = area.flags();
        let r = if flags.contains(MappingFlags::READ) {
            'r'
        } else {
            '-'
        };
        let w = if flags.contains(MappingFlags::WRITE) {
            'w'
        } else {
            '-'
        };
        let x = if flags.contains(MappingFlags::EXECUTE) {
            'x'
        } else {
            '-'
        };
        let shared = is_shared_user_mapping(area.backend());
        let p = if shared { 's' } else { 'p' };
        let name = match area.backend() {
            Backend::Shared(_) => " [shared]",
            Backend::Linear(_) => "",
            Backend::Cow(_) | Backend::File(_) => "",
        };
        let _ = writeln!(
            out,
            "{start:08x}-{end:08x} {r}{w}{x}{p} 00000000 00:00 0{name:>10}",
        );

        if include_smaps {
            let page_size = area.backend().page_size() as usize;
            let mut resident_bytes = 0;
            let mut cursor = area.start();
            while cursor < area.end() {
                let step = page_size.min(area.end().sub_addr(cursor));
                if aspace.page_table().query(cursor).is_ok() {
                    resident_bytes += step;
                }
                cursor += page_size;
            }
            let locked_bytes = aspace.locked_bytes_in_range(area.start(), area.size());
            let _ = writeln!(out, "Size:           {:>8} kB", area.size() / 1024);
            let _ = writeln!(out, "Rss:            {:>8} kB", resident_bytes / 1024);
            let _ = writeln!(out, "Locked:         {:>8} kB", locked_bytes / 1024);
        }
    }

    out
}

fn mempolicy_effective_mask(policy: Mempolicy) -> usize {
    let mask = policy.nodemask & PROC_NUMA_NODEMASK;
    if mask != 0 { mask } else { 1 }
}

fn format_node_list(mask: usize) -> String {
    let mut ranges = Vec::new();
    let mut node = 0usize;
    while node < usize::BITS as usize {
        let bit = 1usize.checked_shl(node as u32).unwrap_or(0);
        if mask & bit == 0 {
            node += 1;
            continue;
        }
        let start = node;
        while node + 1 < usize::BITS as usize {
            let next_bit = 1usize.checked_shl((node + 1) as u32).unwrap_or(0);
            if mask & next_bit == 0 {
                break;
            }
            node += 1;
        }
        if start == node {
            ranges.push(start.to_string());
        } else {
            ranges.push(format!("{start}-{node}"));
        }
        node += 1;
    }
    ranges.join(",")
}

fn first_node(mask: usize) -> usize {
    if mask == 0 {
        0
    } else {
        mask.trailing_zeros() as usize
    }
}

fn numa_policy_text(policy: Mempolicy) -> String {
    let mask = mempolicy_effective_mask(policy);
    let nodes = format_node_list(mask);
    match policy.mode {
        mode if mode == MPOL_BIND as u32 => format!("bind:{nodes}"),
        mode if mode == MPOL_INTERLEAVE as u32 => format!("interleave:{nodes}"),
        mode if mode == MPOL_PREFERRED as u32 || mode == MPOL_PREFERRED_MANY as u32 => {
            format!("prefer:{nodes}")
        }
        mode if mode == MPOL_LOCAL as u32 => "local".into(),
        mode if mode == MPOL_DEFAULT as u32 => "default".into(),
        _ => "default".into(),
    }
}

fn is_user_stack_area(start: usize, end: usize) -> bool {
    let stack_top = crate::config::USER_STACK_TOP;
    let stack_bottom = stack_top.saturating_sub(crate::config::USER_STACK_SIZE);
    start < stack_top && end > stack_bottom
}

fn render_task_numa_maps(
    policy: &MempolicySnapshot,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
) -> String {
    let aspace = aspace_handle.lock();
    let mut out = String::new();

    for area in aspace.areas() {
        if !area.flags().contains(MappingFlags::USER) {
            continue;
        }
        let start = area.start().as_usize();
        let end = start + area.size();
        let policy = policy.policy_for_addr(start);
        let policy_text = numa_policy_text(policy);
        let page_size = area.backend().page_size() as usize;
        let mut resident_pages = 0usize;
        let mut cursor = area.start();
        while cursor < area.end() {
            let step = page_size.min(area.end().sub_addr(cursor));
            if aspace.page_table().query(cursor).is_ok() {
                resident_pages += step.div_ceil(PAGE_SIZE_4K);
            }
            cursor += step;
        }
        let node = first_node(mempolicy_effective_mask(policy));
        let stack = if is_user_stack_area(start, end) {
            " stack"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{start:x} {policy_text}{stack} anon={resident_pages} dirty={resident_pages} \
             N{node}={resident_pages} kernelpagesize_kB={}",
            page_size / 1024
        );
    }

    out
}

/// The /proc/[pid]/fd directory
struct ThreadFdDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
    process_view: bool,
}

struct PreparedFdMagicLink {
    fd: u32,
    task: WeakAxTaskRef,
    process_view: bool,
}

impl SimpleFileOps for PreparedFdMagicLink {
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let authorized_image = proc_fd_image_access(&task, self.process_view)?;
        let description = task
            .as_thread()
            .fd_table()
            .get_description_number(self.fd)
            .map_err(|_| VfsError::NotFound)?;
        let target: FsPathBuf = try_path_into_owned(description.inner.path()?)?;
        validate_proc_fd_image(&task, &authorized_image)?;
        Ok(Cow::Owned(target.as_bytes().to_vec()))
    }

    fn write_all(&self, _data: &[u8]) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

impl SimpleDirOps for ThreadFdDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let Some(task) = self.task.upgrade() else {
            return try_boxed_names(iter::empty());
        };
        let authorized_image = proc_fd_image_access(&task, self.process_view)?;
        let ids = task
            .as_thread()
            .fd_table()
            .try_fd_numbers()?
            .into_iter()
            .map(|id| FsNameBuf::from_vec(id.to_string().into_bytes()).map(Cow::Owned))
            .collect::<VfsResult<Vec<_>>>()?;
        validate_proc_fd_image(&task, &authorized_image)?;
        try_boxed_names(ids.into_iter())
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let authorized_image = proc_fd_image_access(&task, self.process_view)?;
        let fd = str::from_utf8(name.as_bytes())
            .map_err(|_| VfsError::NotFound)?
            .parse::<u32>()
            .map_err(|_| VfsError::NotFound)?;
        task.as_thread()
            .fd_table()
            .get_description_number(fd)
            .map_err(|_| VfsError::NotFound)?;
        validate_proc_fd_image(&task, &authorized_image)?;
        Ok(SimpleFile::try_new_magic_link(
            fs,
            PreparedFdMagicLink {
                fd,
                task: Arc::downgrade(&task),
                process_view: self.process_view,
            },
        )?
        .into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

/// The /proc/[pid]/fdinfo directory
struct ThreadFdInfoDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
    process_view: bool,
}

impl ThreadFdInfoDir {
    fn description_for(&self, name: &FsName) -> VfsResult<Arc<FileDescription>> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let authorized_image = proc_fd_image_access(&task, self.process_view)?;
        let fd = str::from_utf8(name.as_bytes())
            .map_err(|_| VfsError::NotFound)?
            .parse::<usize>()
            .map_err(|_| VfsError::NotFound)?;
        let description = task
            .as_thread()
            .fd_table()
            .get_description_number(u32::try_from(fd).map_err(|_| VfsError::NotFound)?)
            .map_err(|_| VfsError::NotFound)?;
        validate_proc_fd_image(&task, &authorized_image)?;
        Ok(description)
    }

    fn render_fdinfo(description: &FileDescription) -> String {
        let stat = description.inner.stat().ok();
        let mnt_id = stat.map_or(0, |stat| stat.mnt_id);
        let mut out = format!(
            "pos:\t0\nflags:\t{:o}\nmnt_id:\t{}\n",
            description.status_flags(),
            mnt_id
        );
        if let Some(stat) = stat {
            let _ = writeln!(out, "ino:\t{}", stat.ino);
        }
        if let Some(inotify) = description.inner.downcast_ref::<InotifyFile>() {
            out.push_str(&inotify.fdinfo());
        }
        if let Some(fanotify) = description.inner.downcast_ref::<FanotifyFile>() {
            out.push_str(&fanotify.fdinfo());
        }
        if let Some(pidfd) = description.inner.downcast_ref::<PidFd>()
            && let Ok(proc_data) = pidfd.process_data()
        {
            let pid = proc_data.proc.pid();
            let _ = writeln!(out, "Pid:\t{pid}");
            let _ = writeln!(out, "NSpid:\t{pid}");
        }
        out
    }
}

impl SimpleDirOps for ThreadFdInfoDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let Some(task) = self.task.upgrade() else {
            return try_boxed_names(iter::empty());
        };
        let authorized_image = proc_fd_image_access(&task, self.process_view)?;
        let ids = task
            .as_thread()
            .fd_table()
            .try_fd_numbers()?
            .into_iter()
            .map(|id| FsNameBuf::from_vec(id.to_string().into_bytes()).map(Cow::Owned))
            .collect::<VfsResult<Vec<_>>>()?;
        validate_proc_fd_image(&task, &authorized_image)?;
        try_boxed_names(ids.into_iter())
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let description = self.description_for(name)?;
        Ok(SimpleFile::new_regular(fs, move || Ok(Self::render_fdinfo(&description))).into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ProcNamespaceKind {
    Cgroup,
    Ipc,
    Mount,
    Net,
    Pid,
    Time,
    TimeForChildren,
    User,
    Uts,
}

pub(crate) enum ProcNamespaceObject {
    Cgroup(Arc<CgroupNamespace>),
    Ipc(Arc<IpcNamespace>),
    Mount(Arc<MountNamespace>),
    Net(Arc<NetworkNamespace>),
    Pid(Arc<PidNamespace>),
    Time(Arc<TimeNamespace>),
    User(Arc<UserNamespace>),
    Uts(Arc<UtsNamespace>),
}

impl ProcNamespaceObject {
    /// Returns the user namespace which owns this namespace object.
    ///
    /// A non-initial user namespace is itself owned by its parent. The initial
    /// user namespace has no owning namespace and no `NS_GET_USERNS` result.
    pub(crate) fn owner_user_ns(&self) -> Option<Arc<UserNamespace>> {
        match self {
            Self::Cgroup(ns) => Some(ns.owner_user_ns().clone()),
            Self::Ipc(ns) => Some(ns.owner_user_ns().clone()),
            Self::Mount(ns) => Some(ns.owner_user_ns().clone()),
            Self::Net(ns) => Some(ns.owner_user_ns().clone()),
            Self::Pid(ns) => Some(ns.owner_user_ns().clone()),
            Self::Time(ns) => Some(ns.owner_user_ns().clone()),
            Self::User(ns) => ns.parent(),
            Self::Uts(ns) => Some(ns.owner_user_ns().clone()),
        }
    }
}

struct ProcNamespaceFile {
    node: SimpleFsNode,
    fs: Arc<SimpleFs>,
    kind: ProcNamespaceKind,
    object: ProcNamespaceObject,
}

impl ProcNamespaceFile {
    fn new(
        fs: Arc<SimpleFs>,
        kind: ProcNamespaceKind,
        task: &AxTaskRef,
        _process_view: bool,
    ) -> Arc<Self> {
        let thread = task.as_thread();
        let object = match kind {
            ProcNamespaceKind::Cgroup => ProcNamespaceObject::Cgroup(thread.cgroup_ns()),
            ProcNamespaceKind::Ipc => ProcNamespaceObject::Ipc(thread.ipc_ns()),
            ProcNamespaceKind::Mount => ProcNamespaceObject::Mount(thread.mount_ns()),
            ProcNamespaceKind::Net => ProcNamespaceObject::Net(thread.net_ns()),
            ProcNamespaceKind::Pid => ProcNamespaceObject::Pid(thread.pid_ns()),
            ProcNamespaceKind::Time => ProcNamespaceObject::Time(thread.time_ns()),
            ProcNamespaceKind::TimeForChildren => {
                ProcNamespaceObject::Time(thread.time_ns_for_children())
            }
            ProcNamespaceKind::User => ProcNamespaceObject::User(thread.user_ns()),
            ProcNamespaceKind::Uts => ProcNamespaceObject::Uts(thread.uts_ns()),
        };
        Self::from_object(fs, kind, object)
    }

    fn from_object(
        fs: Arc<SimpleFs>,
        kind: ProcNamespaceKind,
        object: ProcNamespaceObject,
    ) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs.clone(),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o444),
            ),
            fs,
            kind,
            object,
        })
    }

    fn namespace_inode(&self) -> Option<u64> {
        match &self.object {
            ProcNamespaceObject::Cgroup(ns) => Some(ns.proc_inode()),
            // IPC owns an independent allocator; reserve the low bits for
            // its typed procfs identity rather than aliasing global process
            // namespace IDs.
            ProcNamespaceObject::Ipc(ns) => Some(0x9_0000_0000 + ns.id().saturating_mul(8) + 1),
            ProcNamespaceObject::Mount(ns) => Some(ns.proc_inode()),
            ProcNamespaceObject::Net(ns) => Some(ns.proc_inode()),
            ProcNamespaceObject::Pid(ns) => Some(ns.proc_inode()),
            ProcNamespaceObject::User(ns) => Some(ns.proc_inode()),
            ProcNamespaceObject::Time(ns) => Some(ns.proc_inode()),
            ProcNamespaceObject::Uts(ns) => Some(ns.proc_inode()),
        }
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for ProcNamespaceFile {
    fn inode(&self) -> u64 {
        self.namespace_inode().unwrap_or_else(|| self.node.inode())
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.node.metadata()?;
        if let Some(inode) = self.namespace_inode() {
            metadata.inode = inode;
        }
        Ok(metadata)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn filesystem(&self) -> &dyn FilesystemOps;

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(0)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::MAGIC_LINK
    }
}

impl FileNodeOps for ProcNamespaceFile {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }

    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::BadFileDescriptor)
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }

    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

impl Pollable for ProcNamespaceFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

struct ThreadNamespaceDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
    process_view: bool,
}

impl SimpleDirOps for ThreadNamespaceDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let Some(task) = self.task.upgrade() else {
            return try_boxed_names(iter::empty());
        };
        drop(proc_image_access(&task, self.process_view)?);
        try_boxed_names(
            [
                b"cgroup".as_slice(),
                b"ipc".as_slice(),
                b"mnt".as_slice(),
                b"net".as_slice(),
                b"pid".as_slice(),
                b"time".as_slice(),
                b"time_for_children".as_slice(),
                b"user".as_slice(),
                b"uts".as_slice(),
            ]
            .into_iter()
            .map(|name| Cow::Borrowed(FsName::new(name))),
        )
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        drop(proc_image_access(&task, self.process_view)?);
        let kind = match name.as_bytes() {
            b"cgroup" => ProcNamespaceKind::Cgroup,
            b"ipc" => ProcNamespaceKind::Ipc,
            b"mnt" => ProcNamespaceKind::Mount,
            b"net" => ProcNamespaceKind::Net,
            b"pid" => ProcNamespaceKind::Pid,
            b"time" => ProcNamespaceKind::Time,
            b"time_for_children" => ProcNamespaceKind::TimeForChildren,
            b"user" => ProcNamespaceKind::User,
            b"uts" => ProcNamespaceKind::Uts,
            _ => return Err(VfsError::NotFound),
        };
        Ok(ProcNamespaceFile::new(self.fs.clone(), kind, &task, self.process_view).into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

/// The /proc/[pid] directory
struct ThreadDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
    show_task_dir: bool,
}

struct ZombieProcessDir {
    fs: Arc<SimpleFs>,
    process: Weak<Process>,
}

/// Resolves the exact unreaped process identity retained by a procfs handle.
///
/// A zombie `Arc` can outlive the process-domain registry entry when a file was
/// opened before `wait(2)` reaped it.  Never render that retained payload
/// without first matching the canonical PID entry and its exact process
/// generation; otherwise a stale handle could continue to expose a reaped
/// zombie or alias a later process which reuses the PID.
fn authoritative_zombie(process: &Process) -> VfsResult<Arc<Process>> {
    let current = process_domain()?
        .registry()
        .get(process.pid())
        .ok_or(VfsError::NotFound)?;
    if !core::ptr::eq(&*current, process)
        || !current.is_zombie()
        || current.zombie_payload().is_none()
    {
        return Err(VfsError::NotFound);
    }
    Ok(current)
}

fn zombie_stat_lifecycle_error(error: VfsError) -> VfsError {
    match error {
        VfsError::NotFound => VfsError::NoSuchProcess,
        error => error,
    }
}

/// Verifies that a retained live-task proc directory still names the exact
/// process published in the PID registry.  The registry pointer check is the
/// generation boundary: a reused numeric PID must not revive an old dentry.
fn authoritative_proc_pid_process(process: &Process) -> AxResult<()> {
    let current = process_domain()?
        .registry()
        .get(process.pid())
        .ok_or(AxError::NoSuchProcess)?;
    if !core::ptr::eq(&*current, process) {
        return Err(AxError::NoSuchProcess);
    }
    Ok(())
}

/// Admits pathname search through a retained `/proc/[pid]` directory only
/// while its exact process identity remains the canonical live or zombie PID
/// entry appropriate to the retained dentry.
///
/// The generic path resolver invokes this check before looking up a named
/// child and while traversing `.`/`..`.  Pointer equality is intentional: a
/// recycled numeric PID must never make an old directory handle search a new
/// process.  The descriptor itself remains usable for metadata and
/// `AT_EMPTY_PATH`; only pathname search through a stale directory is ESRCH.
pub(crate) fn check_proc_pid_dir_search(loc: &axfs_ng_vfs::Location) -> AxResult<()> {
    if let Ok(dir) = loc.entry().downcast::<SimpleDir<ZombieProcessDir>>() {
        let process = dir.ops().process.upgrade().ok_or(AxError::NoSuchProcess)?;
        return authoritative_zombie(&process)
            .map(|_| ())
            .map_err(zombie_stat_lifecycle_error);
    }

    let Ok(dir) = loc.entry().downcast::<SimpleDir<ThreadDir>>() else {
        return Ok(());
    };
    let task = dir.ops().task.upgrade().ok_or(AxError::NoSuchProcess)?;
    let process = task.as_thread().proc_data.proc.clone();
    authoritative_proc_pid_process(&process)
}

/// Reads zombie stat only while the canonical identity and payload remain
/// current.  The second lookup closes the ordinary reap-before-return window;
/// pointer equality keeps a recycled numeric PID from redirecting the handle.
fn read_authoritative_zombie_stat(process: &Process) -> VfsResult<Vec<u8>> {
    let current = authoritative_zombie(process).map_err(zombie_stat_lifecycle_error)?;
    let payload = current.zombie_payload().ok_or(VfsError::NoSuchProcess)?;
    let stat = render_zombie_stat(&current)?.into_bytes();
    let current_after = authoritative_zombie(process).map_err(zombie_stat_lifecycle_error)?;
    let payload_after = current_after
        .zombie_payload()
        .ok_or(VfsError::NoSuchProcess)?;
    if !Arc::ptr_eq(&payload, &payload_after) {
        return Err(VfsError::NoSuchProcess);
    }
    Ok(stat)
}

pub(crate) enum ProcDirProcess {
    NotProcDir,
    Live(Arc<ProcessData>),
    Stale,
}

pub(crate) enum ProcNamespaceTarget {
    NotNamespace,
    Live(ProcNamespaceKind, ProcNamespaceObject),
}

pub(crate) fn process_data_from_proc_dir(loc: &axfs_ng_vfs::Location) -> ProcDirProcess {
    let Ok(dir) = loc.entry().downcast::<SimpleDir<ThreadDir>>() else {
        return ProcDirProcess::NotProcDir;
    };
    dir.ops()
        .task
        .upgrade()
        .map_or(ProcDirProcess::Stale, |task| {
            ProcDirProcess::Live(task.as_thread().proc_data.clone())
        })
}

pub(crate) fn namespace_target_from_proc_file(loc: &axfs_ng_vfs::Location) -> ProcNamespaceTarget {
    let Ok(file) = loc.entry().downcast::<ProcNamespaceFile>() else {
        return ProcNamespaceTarget::NotNamespace;
    };
    let object = match &file.object {
        ProcNamespaceObject::Cgroup(ns) => ProcNamespaceObject::Cgroup(ns.clone()),
        ProcNamespaceObject::Ipc(ns) => ProcNamespaceObject::Ipc(ns.clone()),
        ProcNamespaceObject::Mount(ns) => ProcNamespaceObject::Mount(ns.clone()),
        ProcNamespaceObject::Net(ns) => ProcNamespaceObject::Net(ns.clone()),
        ProcNamespaceObject::Pid(ns) => ProcNamespaceObject::Pid(ns.clone()),
        ProcNamespaceObject::Time(ns) => ProcNamespaceObject::Time(ns.clone()),
        ProcNamespaceObject::User(ns) => ProcNamespaceObject::User(ns.clone()),
        ProcNamespaceObject::Uts(ns) => ProcNamespaceObject::Uts(ns.clone()),
    };
    ProcNamespaceTarget::Live(file.kind, object)
}

/// Returns the namespace object retained by an opened `/proc/*/ns/net` file.
/// BPF link creation resolves this once and retains the resulting namespace;
/// it never reuses the caller's numeric descriptor from packet context.
pub(crate) fn bpf_network_namespace_from_fd(fd: i32) -> AxResult<Arc<NetworkNamespace>> {
    let file = crate::file::get_typed_file::<crate::file::File>(fd)?;
    let ProcNamespaceTarget::Live(ProcNamespaceKind::Net, ProcNamespaceObject::Net(namespace)) =
        namespace_target_from_proc_file(file.inner().location())
    else {
        return Err(AxError::InvalidInput);
    };
    Ok(namespace)
}

pub(crate) fn proc_namespace_location_from_object(
    template: &Location,
    kind: ProcNamespaceKind,
    object: ProcNamespaceObject,
) -> VfsResult<Location> {
    let parent = template.entry().parent();
    let name = match kind {
        ProcNamespaceKind::Cgroup => "cgroup",
        ProcNamespaceKind::Ipc => "ipc",
        ProcNamespaceKind::Mount => "mnt",
        ProcNamespaceKind::Net => "net",
        ProcNamespaceKind::Pid => "pid",
        ProcNamespaceKind::Time => "time",
        ProcNamespaceKind::TimeForChildren => "time_for_children",
        ProcNamespaceKind::User => "user",
        ProcNamespaceKind::Uts => "uts",
    };
    let template_file = template.entry().downcast::<ProcNamespaceFile>()?;
    let file = ProcNamespaceFile::from_object(template_file.fs.clone(), kind, object);
    let entry = DirEntry::new_file(
        FileNode::new(file),
        NodeType::RegularFile,
        Reference::try_new(parent, FsName::new(name.as_bytes()))?,
    );
    Ok(Location::new(template.mountpoint().clone(), entry))
}

fn parse_timens_offset_line(line: &str) -> VfsResult<Option<(u32, i64, u32)>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut fields = trimmed.split_whitespace();
    let clock = fields.next().ok_or(VfsError::InvalidInput)?;
    let secs = fields
        .next()
        .ok_or(VfsError::InvalidInput)?
        .parse::<i64>()
        .map_err(|_| VfsError::InvalidInput)?;
    let nsecs = fields
        .next()
        .ok_or(VfsError::InvalidInput)?
        .parse::<u32>()
        .map_err(|_| VfsError::InvalidInput)?;
    if fields.next().is_some() || nsecs >= 1_000_000_000 {
        return Err(VfsError::InvalidInput);
    }

    let clock = match clock {
        "monotonic" => CLOCK_MONOTONIC,
        "boottime" => CLOCK_BOOTTIME,
        value => match value.parse::<u32>() {
            Ok(value) if value == CLOCK_MONOTONIC || value == CLOCK_BOOTTIME => value,
            _ => return Err(VfsError::InvalidInput),
        },
    };
    Ok(Some((clock, secs, nsecs)))
}

fn render_timens_offsets(task: &AxTaskRef) -> Vec<u8> {
    task.as_thread().time_ns_for_children().render_offsets()
}

fn write_timens_offsets(task: &AxTaskRef, data: &[u8]) -> VfsResult<()> {
    if data.len() >= PAGE_SIZE_4K {
        return Err(VfsError::InvalidInput);
    }
    let text = str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let actor_cred = current_file_operation_security_credential().ok_or(VfsError::Io)?;
    let time_ns = task.as_thread().time_ns_for_children();
    if !ns_capable(
        actor_cred.as_ref(),
        time_ns.owner_user_ns(),
        linux_raw_sys::general::CAP_SYS_TIME,
    ) {
        return Err(VfsError::PermissionDenied);
    }

    let mut parsed = Vec::new();
    for line in text.lines() {
        if let Some(offset) = parse_timens_offset_line(line)? {
            parsed.push(offset);
        }
    }
    if parsed.is_empty() {
        return Err(VfsError::InvalidInput);
    }

    for (clock, secs, nsecs) in parsed {
        match clock {
            CLOCK_MONOTONIC => time_ns.set_monotonic_offset(secs, nsecs),
            CLOCK_BOOTTIME => time_ns.set_boottime_offset(secs, nsecs),
            _ => return Err(VfsError::InvalidInput),
        }
    }
    Ok(())
}

struct ProcPagemapFile {
    node: SimpleFsNode,
    aspace: Arc<Mutex<AddrSpace>>,
    show_pfn: bool,
}

impl ProcPagemapFile {
    fn new(fs: Arc<SimpleFs>, aspace: Arc<Mutex<AddrSpace>>, show_pfn: bool) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o444),
            ),
            aspace,
            show_pfn,
        })
    }

    fn pagemap_entry(&self, vpn: u64) -> u64 {
        let Some(vaddr) = vpn
            .checked_mul(PAGE_SIZE_4K as u64)
            .and_then(|addr| usize::try_from(addr).ok())
            .map(VirtAddr::from)
        else {
            return 0;
        };
        let aspace = self.aspace.lock();
        match aspace.page_table().query(vaddr) {
            Ok((paddr, ..)) => {
                let pfn = if self.show_pfn {
                    paddr.as_usize() as u64 / PAGE_SIZE_4K as u64
                } else {
                    0
                };
                (1u64 << 63) | pfn
            }
            Err(_) => 0,
        }
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for ProcPagemapFile {
    fn inode(&self) -> u64;

    fn metadata(&self) -> VfsResult<Metadata>;

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn filesystem(&self) -> &dyn FilesystemOps;

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(0)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

impl FileNodeOps for ProcPagemapFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let mut written = 0;
        let mut entry_index = offset / PROC_PAGEMAP_ENTRY_BYTES;
        let mut entry_offset = (offset % PROC_PAGEMAP_ENTRY_BYTES) as usize;

        while written < buf.len() {
            let entry = self.pagemap_entry(entry_index).to_le_bytes();
            let copy_len =
                (PROC_PAGEMAP_ENTRY_BYTES as usize - entry_offset).min(buf.len() - written);
            buf[written..written + copy_len]
                .copy_from_slice(&entry[entry_offset..entry_offset + copy_len]);
            written += copy_len;
            entry_index += 1;
            entry_offset = 0;
        }

        Ok(written)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }

    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::BadFileDescriptor)
    }

    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }

    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

impl Pollable for ProcPagemapFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}

impl SimpleDirOps for ZombieProcessDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        match self.process.upgrade() {
            Some(process) => match authoritative_zombie(&process) {
                Ok(_) => try_boxed_names(iter::once(Cow::Borrowed(FsName::new(b"stat")))),
                Err(VfsError::NotFound) => Err(VfsError::NotFound),
                Err(error) => Err(error),
            },
            None => Err(VfsError::NotFound),
        }
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        if name.as_bytes() != b"stat" {
            return Err(VfsError::NotFound);
        }
        let fs = self.fs.clone();
        let process = self.process.upgrade().ok_or(VfsError::NotFound)?;
        let _ = authoritative_zombie(&process)?;
        Ok(SimpleFile::new_regular(fs, move || read_authoritative_zombie_stat(&process)).into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

impl SimpleDirOps for ThreadDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let process = task.as_thread().proc_data.proc.clone();
        authoritative_proc_pid_process(&process).map_err(|error| match error {
            AxError::NoSuchProcess => VfsError::NotFound,
            error => error,
        })?;
        try_boxed_names(
            [
                Some(b"stat".as_slice()),
                Some(b"status".as_slice()),
                Some(b"uid_map".as_slice()),
                Some(b"gid_map".as_slice()),
                Some(b"setgroups".as_slice()),
                Some(b"limits".as_slice()),
                Some(b"oom_score_adj".as_slice()),
                Some(b"cgroup".as_slice()),
                Some(b"cpuset".as_slice()),
                self.show_task_dir.then_some(b"task".as_slice()),
                Some(b"maps".as_slice()),
                Some(b"smaps".as_slice()),
                Some(b"numa_maps".as_slice()),
                Some(b"pagemap".as_slice()),
                Some(b"mounts".as_slice()),
                Some(b"mountinfo".as_slice()),
                Some(b"cmdline".as_slice()),
                Some(b"timerslack_ns".as_slice()),
                Some(b"timens_offsets".as_slice()),
                Some(b"comm".as_slice()),
                Some(b"exe".as_slice()),
                Some(b"fd".as_slice()),
                Some(b"fdinfo".as_slice()),
                Some(b"ns".as_slice()),
            ]
            .into_iter()
            .flatten()
            .map(|name| Cow::Borrowed(FsName::new(name))),
        )
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let process_view = self.show_task_dir;
        Ok(match name.as_bytes() {
            b"stat" => {
                SimpleFile::new_regular(fs, move || Ok(render_task_stat(&task)?.into_bytes()))
                    .into()
            }
            b"status" => SimpleFile::try_new_regular_with_open_credential(fs, move || {
                let viewer = current_file_operation_security_credential().ok_or(VfsError::Io)?;
                task_status(&task, process_view, viewer.user_ns())
            })?
            .into(),
            b"uid_map" => {
                let subject = proc_subject_cred(&task, process_view);
                let ids = subject.ids();
                ProcUserNamespaceFile::try_new(
                    fs,
                    subject.user_ns().clone(),
                    ProcUserNamespaceFileKind::UidMap,
                    ids.euid,
                    ids.egid,
                )?
                .into()
            }
            b"gid_map" => {
                let subject = proc_subject_cred(&task, process_view);
                let ids = subject.ids();
                ProcUserNamespaceFile::try_new(
                    fs,
                    subject.user_ns().clone(),
                    ProcUserNamespaceFileKind::GidMap,
                    ids.euid,
                    ids.egid,
                )?
                .into()
            }
            b"setgroups" => {
                let subject = proc_subject_cred(&task, process_view);
                let ids = subject.ids();
                ProcUserNamespaceFile::try_new(
                    fs,
                    subject.user_ns().clone(),
                    ProcUserNamespaceFileKind::Setgroups,
                    ids.euid,
                    ids.egid,
                )?
                .into()
            }
            b"limits" => SimpleFile::new_regular(fs, move || Ok(render_task_limits(&task))).into(),
            b"oom_score_adj" => SimpleFile::new_regular(
                fs,
                RwFile::new_process_writable(move |req| match req {
                    SimpleFileOperation::Read => Ok(Some(
                        task.as_thread().oom_score_adj().to_string().into_bytes(),
                    )),
                    SimpleFileOperation::Write(data) => {
                        if !data.is_empty() {
                            drop(proc_image_access(&task, process_view)?);
                            let value = parse_oom_score_adj_value(data)?;
                            task.as_thread().set_oom_score_adj(value);
                        }
                        Ok(None)
                    }
                }),
            )
            .into(),
            b"cgroup" => {
                let pid = task.as_thread().proc_data.proc.pid();
                let process = task.as_thread().proc_data.proc.clone();
                SimpleFile::new_regular(fs, move || {
                    let roots = current().as_thread().cgroup_ns().roots().clone();
                    Ok(proc_cgroup_membership(pid, &process, &roots))
                })
                .into()
            }
            b"cpuset" => {
                let pid = task.as_thread().proc_data.proc.pid();
                let process = task.as_thread().proc_data.proc.clone();
                SimpleFile::new_regular(fs, move || {
                    let roots = current().as_thread().cgroup_ns().roots().clone();
                    Ok(proc_cpuset_membership(pid, &process, &roots))
                })
                .into()
            }
            b"task" if self.show_task_dir => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ProcessTaskDir {
                    fs,
                    process: Arc::downgrade(&task.as_thread().proc_data.proc),
                }),
            )
            .into(),
            b"maps" => {
                let aspace = proc_image_access(&task, process_view)?.into_aspace();
                SimpleFile::new_regular(fs, move || Ok(render_task_maps(&aspace, false))).into()
            }
            b"smaps" => {
                let aspace = proc_image_access(&task, process_view)?.into_aspace();
                SimpleFile::new_regular(fs, move || Ok(render_task_maps(&aspace, true))).into()
            }
            b"numa_maps" => {
                let aspace = proc_image_access(&task, process_view)?.into_aspace();
                let proc_data = task.as_thread().proc_data.clone();
                let policy = proc_data
                    .try_mempolicy_snapshot_for_image(&aspace)
                    .map_err(|err| match err {
                        AxError::NoMemory => VfsError::NoMemory,
                        _ => VfsError::PermissionDenied,
                    })?;
                SimpleFile::new_regular(fs, move || Ok(render_task_numa_maps(&policy, &aspace)))
                    .into()
            }
            b"pagemap" => {
                let aspace = proc_image_access(&task, process_view)?.into_aspace();
                let show_pfn = current()
                    .as_thread()
                    .has_effective_capability(CAP_SYS_ADMIN);
                ProcPagemapFile::new(fs, aspace, show_pfn).into()
            }
            b"mounts" => SimpleFile::new_regular(fs, render_mounts).into(),
            b"mountinfo" => SimpleFile::new_regular(fs, render_mountinfo).into(),
            b"cmdline" => SimpleFile::new_regular(fs, move || {
                let cmdline = task.as_thread().proc_data.cmdline.read();
                let mut buf = Vec::new();
                for arg in cmdline.iter() {
                    buf.extend_from_slice(arg);
                    buf.push(0);
                }
                Ok(buf)
            })
            .into(),
            b"timerslack_ns" => SimpleFile::try_new_regular_with_open_credential(
                fs,
                RwFile::new_process_writable(move |req| match req {
                    SimpleFileOperation::Read => {
                        let actor =
                            current_file_operation_security_credential().ok_or(VfsError::Io)?;
                        check_timerslack_access(&task, actor.as_ref())?;
                        Ok(Some(
                            format!("{}\n", task.as_thread().proc_data.timerslack_ns())
                                .into_bytes(),
                        ))
                    }
                    SimpleFileOperation::Write(data) => {
                        let actor =
                            current_file_operation_security_credential().ok_or(VfsError::Io)?;
                        check_timerslack_access(&task, actor.as_ref())?;
                        if !data.is_empty() {
                            let value = str::from_utf8(data)
                                .ok()
                                .map(str::trim)
                                .and_then(|it| it.parse::<usize>().ok())
                                .ok_or(VfsError::InvalidInput)?;
                            task.as_thread().proc_data.set_timerslack_ns(value);
                        }
                        Ok(None)
                    }
                }),
            )?
            .into(),
            b"timens_offsets" => SimpleFile::try_new_regular_with_open_credential(
                fs,
                RwFile::new_process_writable(move |req| match req {
                    SimpleFileOperation::Read => Ok(Some(render_timens_offsets(&task))),
                    SimpleFileOperation::Write(data) => {
                        write_timens_offsets(&task, data)?;
                        Ok(None)
                    }
                }),
            )?
            .into(),
            b"comm" => SimpleFile::new_regular(
                fs,
                RwFile::new_process_writable(move |req| match req {
                    SimpleFileOperation::Read => {
                        let name = task.try_name().map_err(|_| VfsError::NoMemory)?;
                        let copy_len = name.len().min(15);
                        let mut bytes = Vec::with_capacity(copy_len + 1);
                        bytes.extend_from_slice(&name.as_bytes()[..copy_len]);
                        bytes.push(b'\n');
                        Ok(Some(bytes))
                    }
                    SimpleFileOperation::Write(data) => {
                        if !data.is_empty() {
                            if !Arc::ptr_eq(
                                &current().as_thread().proc_data.proc,
                                &task.as_thread().proc_data.proc,
                            ) {
                                return Err(VfsError::InvalidInput);
                            }
                            let mut input = [0; 16];
                            let data = data.strip_suffix(b"\n").unwrap_or(data);
                            let copy_len = data.len().min(15);
                            input[..copy_len].copy_from_slice(&data[..copy_len]);
                            task.set_name(
                                CStr::from_bytes_until_nul(&input)
                                    .map_err(|_| VfsError::InvalidInput)?
                                    .to_str()
                                    .map_err(|_| VfsError::InvalidInput)?,
                            )
                            .map_err(|_| VfsError::NoMemory)?;
                        }
                        Ok(None)
                    }
                }),
            )
            .into(),
            b"exe" => {
                drop(proc_image_access(&task, process_view)?);
                SimpleFile::new_magic_link(fs, move || {
                    let image = proc_image_access(&task, process_view)?.into_aspace();
                    let proc_data = &task.as_thread().proc_data;
                    if proc_data.exec_in_progress() {
                        return Err(VfsError::PermissionDenied);
                    }
                    let path = proc_data.exe_path.read().as_bytes().to_vec();
                    if proc_data.exec_in_progress() || !proc_data.image_matches(&image) {
                        return Err(VfsError::PermissionDenied);
                    }
                    Ok(path)
                })
                .into()
            }
            b"fd" => SimpleDir::new_maker(fs.clone(), {
                drop(proc_fd_image_access(&task, process_view)?);
                Arc::new(ThreadFdDir {
                    fs,
                    task: Arc::downgrade(&task),
                    process_view,
                })
            })
            .into(),
            b"fdinfo" => SimpleDir::new_maker(fs.clone(), {
                drop(proc_fd_image_access(&task, process_view)?);
                Arc::new(ThreadFdInfoDir {
                    fs,
                    task: Arc::downgrade(&task),
                    process_view,
                })
            })
            .into(),
            b"ns" => SimpleDir::new_maker(fs.clone(), {
                drop(proc_image_access(&task, process_view)?);
                Arc::new(ThreadNamespaceDir {
                    fs,
                    task: Arc::downgrade(&task),
                    process_view,
                })
            })
            .into(),
            _ => return Err(VfsError::NotFound),
        })
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

/// Handles /proc/[pid] & /proc/self
struct ProcFsHandler(Arc<SimpleFs>);

impl SimpleDirOps for ProcFsHandler {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let processes = process_domain()?
            .registry()
            .try_processes()
            .map_err(process_error)?;
        let capacity = processes.len().checked_add(2).ok_or(VfsError::NoMemory)?;
        let mut names = Vec::new();
        names
            .try_reserve_exact(capacity)
            .map_err(|_| VfsError::NoMemory)?;
        for process in processes {
            if process.is_zombie() && process.zombie_payload().is_none() {
                continue;
            }
            names.push(Cow::Owned(FsNameBuf::from_vec(
                try_pid_name(process.pid())?.into_bytes(),
            )?));
        }
        names.push(Cow::Borrowed(FsName::new(b"self")));
        names.push(Cow::Borrowed(FsName::new(b"bpf_stats")));
        try_boxed_names(names.into_iter())
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        if name.as_bytes() == b"bpf_stats" {
            return ProcBpfStatsFile::try_new(self.0.clone()).map(Into::into);
        }
        if name.as_bytes() == b"self" {
            return Ok(SimpleFile::new(self.0.clone(), NodeType::Symlink, || {
                Ok(current().as_thread().proc_data.proc.pid().to_string())
            })
            .into());
        }

        let pid = str::from_utf8(name.as_bytes())
            .map_err(|_| VfsError::NotFound)?
            .parse::<u32>()
            .map_err(|_| VfsError::NotFound)?;
        if let Ok(task) = proc_task_for_pid(pid) {
            return Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
                self.0.clone(),
                Arc::new(ThreadDir {
                    fs: self.0.clone(),
                    task: Arc::downgrade(&task),
                    show_task_dir: true,
                }),
            )));
        }

        let process = get_process_including_zombie(pid).map_err(|_| VfsError::NotFound)?;
        if !process.is_zombie() || process.zombie_payload().is_none() {
            return Err(VfsError::NotFound);
        }
        Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
            self.0.clone(),
            Arc::new(ZombieProcessDir {
                fs: self.0.clone(),
                process: Arc::downgrade(&process),
            }),
        )))
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    fn write_proc_u32(data: &[u8]) -> VfsResult<u32> {
        str::from_utf8(data)
            .ok()
            .map(str::trim)
            .and_then(|it| it.parse::<u32>().ok())
            .ok_or(VfsError::InvalidInput)
    }

    fn write_proc_usize(data: &[u8]) -> VfsResult<usize> {
        str::from_utf8(data)
            .ok()
            .map(str::trim)
            .and_then(|it| it.parse::<usize>().ok())
            .ok_or(VfsError::InvalidInput)
    }

    fn write_proc_i32(data: &[u8]) -> VfsResult<i32> {
        str::from_utf8(data)
            .ok()
            .map(str::trim)
            .and_then(|it| it.parse::<i32>().ok())
            .ok_or(VfsError::InvalidInput)
    }

    fn is_proc_truncate_write(data: &[u8]) -> bool {
        data.iter().all(|byte| byte.is_ascii_whitespace())
    }

    fn proc_net_dev_snapshot() -> String {
        let mut output = concat!(
            "Inter-|   Receive                                                |  Transmit\n",
            " face |bytes    packets errs drop fifo frame compressed multicast|",
            "bytes    packets errs drop fifo colls carrier compressed\n",
        )
        .to_string();
        let net_ns = current().as_thread().net_ns();
        for (name, stats) in net_ns.stack().device_stats() {
            let _ = writeln!(
                output,
                "{name:>6}: {rx_bytes:>7} {rx_packets:>7} {rx_errors:>4} {rx_dropped:>4} 0 0 0 0 \
                 {tx_bytes:>8} {tx_packets:>7} {tx_errors:>4} {tx_dropped:>4} 0 0 0 0",
                rx_bytes = stats.rx_bytes,
                rx_packets = stats.rx_packets,
                rx_errors = stats.rx_errors,
                rx_dropped = stats.rx_dropped,
                tx_bytes = stats.tx_bytes,
                tx_packets = stats.tx_packets,
                tx_errors = stats.tx_errors,
                tx_dropped = stats.tx_dropped,
            );
        }
        output
    }

    fn proc_uts_write_value(data: &[u8]) -> Option<&[u8]> {
        if is_proc_truncate_write(data) {
            return None;
        }
        let len = data
            .iter()
            .position(|&b| b == b'\n' || b == 0)
            .unwrap_or(data.len());
        Some(&data[..len])
    }

    let mut root = DirMapping::new();
    root.add("mounts", SimpleFile::new_regular(fs.clone(), render_mounts));
    root.add(
        "mountinfo",
        SimpleFile::new_regular(fs.clone(), render_mountinfo),
    );
    root.add("sysvipc", {
        let mut sysvipc = DirMapping::new();
        sysvipc.add(
            "msg",
            SimpleFile::new_regular(fs.clone(), || Ok(sysvipc_msg_snapshot())),
        );
        sysvipc.add(
            "shm",
            SimpleFile::new_regular(fs.clone(), sysvipc_shm_snapshot),
        );
        sysvipc.add(
            "sem",
            SimpleFile::new_regular(fs.clone(), || Ok(sysvipc_sem_snapshot())),
        );
        SimpleDir::new_maker(fs.clone(), Arc::new(sysvipc))
    });
    root.add(
        "meminfo",
        SimpleFile::new_regular(fs.clone(), || Ok(real_meminfo())),
    );
    root.add(
        "memory_pressure",
        SimpleFile::new_regular(fs.clone(), || Ok(real_memory_pressure())),
    );
    root.add(
        "cgroups",
        SimpleFile::new_regular(fs.clone(), || Ok(proc_cgroups_snapshot())),
    );
    root.add(
        "swaps",
        SimpleFile::new_regular(fs.clone(), || Ok(PROC_SWAPS_HEADER)),
    );
    root.add(
        "meminfo2",
        SimpleFile::new_regular(fs.clone(), || {
            let allocator = axalloc::global_allocator();
            Ok(format!("{:?}\n", allocator.usages()))
        }),
    );
    root.add(
        "io_stats",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o444),
            || -> VfsResult<Vec<u8>> { Ok(render_proc_io_stats()) },
        ),
    );
    root.add(
        "sched",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o444),
            || -> VfsResult<Vec<u8>> { Ok(render_proc_sched_profile()) },
        ),
    );
    #[cfg(feature = "mm-lock-diagnostics")]
    root.add(
        "mm_lock_stats",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o444),
            || -> VfsResult<Vec<u8>> { Ok(render_proc_mm_lock_stats()) },
        ),
    );
    #[cfg(feature = "asid-switch-diagnostics")]
    root.add(
        "asid_switch_stats",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o444),
            || -> VfsResult<Vec<u8>> { Ok(render_proc_asid_switch_stats()) },
        ),
    );
    #[cfg(feature = "pmu-diagnostics")]
    root.add(
        "pmu_capabilities",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o444),
            || -> VfsResult<Vec<u8>> { Ok(render_proc_pmu_capabilities()) },
        ),
    );
    #[cfg(feature = "test-io-control")]
    root.add(
        "io_test_control",
        super::io_test_control::new_file(fs.clone()),
    );
    #[cfg(feature = "test-io-control")]
    root.add(
        "bpf_executor_control",
        SimpleFile::new_regular_with_permission(
            fs.clone(),
            NodePermission::from_bits_truncate(0o600),
            RwFile::new(bpf_executor_control_operation),
        ),
    );
    root.add(
        "cpuinfo",
        SimpleFile::new_regular(fs.clone(), || {
            let num_cpus = axhal::cpu_num();
            let mut out = String::new();
            for i in 0..num_cpus {
                if i > 0 {
                    out.push('\n');
                }
                let _ = writeln!(out, "processor\t: {i}");
                #[cfg(target_arch = "x86_64")]
                if axhal::asm::user_shadow_stack_enabled() {
                    // CET is committed only after every online CPU accepted
                    // it, so this flag is never a per-reader capability lie.
                    let _ = writeln!(out, "flags\t\t: user_shstk");
                }
            }
            Ok(out)
        }),
    );
    root.add(
        "key-users",
        SimpleFile::new_regular(fs.clone(), key_users_snapshot),
    );
    root.add(
        "version",
        SimpleFile::new_regular(fs.clone(), || Ok(proc_version_string()?.into_bytes())),
    );
    root.add(
        "uptime",
        SimpleFile::new_regular(fs.clone(), || {
            let uptime = current()
                .as_thread()
                .time_ns()
                .apply_boottime_offset(axhal::time::monotonic_time());
            let secs = uptime.as_secs();
            let centisecs = uptime.subsec_nanos() / 10_000_000;
            let idle = axtask::idle_time();
            let idle_secs = idle.as_secs();
            let idle_centisecs = idle.subsec_nanos() / 10_000_000;
            Ok(format!(
                "{secs}.{centisecs:02} {idle_secs}.{idle_centisecs:02}\n"
            ))
        }),
    );
    root.add(
        "cmdline",
        // The supported boot paths do not currently preserve firmware/QEMU
        // command-line bytes. An empty line is honest when no source exists.
        SimpleFile::new_regular(fs.clone(), || Ok("\n")),
    );
    root.add("net", {
        let mut net = DirMapping::new();
        net.add(
            "dev",
            SimpleFile::new_regular(fs.clone(), || Ok(proc_net_dev_snapshot())),
        );
        SimpleDir::new_maker(fs.clone(), Arc::new(net))
    });

    root.add("sys", {
        let mut sys = DirMapping::new();

        sys.add("fs", {
            let mut fs_dir = DirMapping::new();

            fs_dir.add(
                "pipe-max-size",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(format!("{}\n", pipe::pipe_max_size()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_u32(data)? as usize;
                            pipe::set_pipe_max_size(value).map_err(LinuxError::from)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            fs_dir.add(
                "lease-break-time",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(lease::formatted_lease_break_time().into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_u32(data)?;
                            lease::set_lease_break_time_secs(value);
                            Ok(None)
                        }
                    }),
                ),
            );
            fs_dir.add(
                "aio-nr",
                SimpleFile::new_regular(fs.clone(), || Ok(alloc::format!("{}\n", aio_nr()))),
            );
            fs_dir.add(
                "aio-max-nr",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", aio_max_nr()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_usize(data)?;
                            set_aio_max_nr(value);
                            Ok(None)
                        }
                    }),
                ),
            );
            fs_dir.add(
                "nr_open",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", nr_open_limit()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_usize(data)? as u64;
                            if !set_nr_open_limit(value) {
                                return Err(VfsError::InvalidInput);
                            }
                            Ok(None)
                        }
                    }),
                ),
            );
            fs_dir.add("mqueue", {
                let mut mqueue = DirMapping::new();
                mqueue.add(
                    "queues_max",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                Ok(Some(alloc::format!("{}\n", mq_queues_max()).into_bytes()))
                            }
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_mq_queues_max(value);
                                Ok(None)
                            }
                        }),
                    ),
                );
                mqueue.add(
                    "msg_max",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                Ok(Some(alloc::format!("{}\n", mq_msg_max()).into_bytes()))
                            }
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_mq_msg_max(value);
                                Ok(None)
                            }
                        }),
                    ),
                );
                mqueue.add(
                    "msgsize_max",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                Ok(Some(alloc::format!("{}\n", mq_msgsize_max()).into_bytes()))
                            }
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_mq_msgsize_max(value);
                                Ok(None)
                            }
                        }),
                    ),
                );
                SimpleDir::new_maker(fs.clone(), Arc::new(mqueue))
            });
            fs_dir.add("inotify", {
                let mut inotify = DirMapping::new();
                inotify.add(
                    "max_queued_events",
                    SimpleFile::new_regular(fs.clone(), || {
                        Ok(format!("{}\n", crate::file::inotify::MAX_QUEUED_EVENTS).into_bytes())
                    }),
                );

                SimpleDir::new_maker(fs.clone(), Arc::new(inotify))
            });
            fs_dir.add("fanotify", {
                let mut fanotify = DirMapping::new();
                fanotify.add(
                    "max_queued_events",
                    SimpleFile::new_regular(fs.clone(), || {
                        Ok(format!("{}\n", crate::file::fanotify::MAX_QUEUED_EVENTS).into_bytes())
                    }),
                );
                fanotify.add(
                    "max_user_groups",
                    SimpleFile::new_regular(fs.clone(), || {
                        Ok(format!("{}\n", crate::file::fanotify::MAX_USER_GROUPS).into_bytes())
                    }),
                );
                fanotify.add(
                    "max_user_marks",
                    SimpleFile::new_regular(fs.clone(), || {
                        Ok(format!("{}\n", crate::file::fanotify::MAX_USER_MARKS).into_bytes())
                    }),
                );

                SimpleDir::new_maker(fs.clone(), Arc::new(fanotify))
            });

            SimpleDir::new_maker(fs.clone(), Arc::new(fs_dir))
        });

        sys.add("vm", {
            let mut vm = DirMapping::new();

            vm.add(
                "overcommit_memory",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", overcommit_memory_policy()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_u32(data)?;
                            set_overcommit_memory_policy(value).map_err(LinuxError::from)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            vm.add(
                "overcommit_ratio",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", overcommit_ratio()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_u32(data)?;
                            set_overcommit_ratio(value);
                            Ok(None)
                        }
                    }),
                ),
            );

            SimpleDir::new_maker(fs.clone(), Arc::new(vm))
        });

        sys.add("kernel", {
            let mut kernel = DirMapping::new();

            kernel.add(
                "arch",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o444),
                    || Ok(format!("{}\n", current_machine_string()?).into_bytes()),
                ),
            );
            kernel.add(
                "ostype",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o444),
                    || Ok(format!("{}\n", current_sysname_string()?).into_bytes()),
                ),
            );
            kernel.add(
                "osrelease",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o444),
                    || Ok(format!("{}\n", current_release_string()?).into_bytes()),
                ),
            );
            kernel.add(
                "version",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o444),
                    || Ok(format!("{}\n", current_version_string()?).into_bytes()),
                ),
            );
            kernel.add(
                "pid_max",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new_root_writable(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", current().as_thread().pid_ns().pid_max())
                                .into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let task = current();
                            let thread = task.as_thread();
                            let pid_ns = thread.pid_ns();
                            pid_ns
                                .try_set_pid_max(write_proc_u32(data)?)
                                .map_err(LinuxError::from)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "domainname",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o644),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            let mut value = current_domainname_bytes()?;
                            value.push(b'\n');
                            Ok(Some(value))
                        }
                        SimpleFileOperation::Write(data) => {
                            if !current_can_administer_uts() {
                                return Err(VfsError::PermissionDenied);
                            }
                            if let Some(value) = proc_uts_write_value(data) {
                                set_domainname_bytes(value)?;
                            }
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "hostname",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o644),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            let mut value = current_hostname_bytes()?;
                            value.push(b'\n');
                            Ok(Some(value))
                        }
                        SimpleFileOperation::Write(data) => {
                            if !current_can_administer_uts() {
                                return Err(VfsError::PermissionDenied);
                            }
                            if let Some(value) = proc_uts_write_value(data) {
                                set_hostname_bytes(value)?;
                            }
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "sched_rr_timeslice_ms",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", sched_rr_timeslice_ms()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_i32(data)?;
                            set_sched_rr_timeslice_ms(value);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "kexec_load_disabled",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", u8::from(kexec_load_disabled())).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            match write_proc_i32(data)? {
                                0 if !kexec_load_disabled() => Ok(None),
                                1 => disable_kexec_load().map(|()| None).map_err(VfsError::from),
                                _ => Err(VfsError::InvalidInput),
                            }
                        }
                    }),
                ),
            );
            for (name, read, write) in [
                (
                    "sched_util_clamp_min",
                    sched_util_clamp_min as fn() -> u16,
                    set_sched_util_clamp_min as fn(u16) -> VfsResult<()>,
                ),
                (
                    "sched_util_clamp_max",
                    sched_util_clamp_max as fn() -> u16,
                    set_sched_util_clamp_max as fn(u16) -> VfsResult<()>,
                ),
                (
                    "sched_util_clamp_min_rt_default",
                    sched_util_clamp_min_rt_default as fn() -> u16,
                    set_sched_util_clamp_min_rt_default as fn(u16) -> VfsResult<()>,
                ),
            ] {
                kernel.add(
                    name,
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                Ok(Some(alloc::format!("{}\n", read()).into_bytes()))
                            }
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_i32(data)?;
                                if !(0..=1024).contains(&value) {
                                    return Err(VfsError::InvalidInput);
                                }
                                write(value as u16)?;
                                Ok(None)
                            }
                        }),
                    ),
                );
            }
            kernel.add(
                "perf_event_paranoid",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", perf_event_paranoid()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            set_perf_event_paranoid(write_proc_i32(data)?)
                                .map_err(LinuxError::from)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "perf_event_mlock_kb",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", perf_event_mlock_kb()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            set_perf_event_mlock_kb(write_proc_u32(data)?)
                                .map_err(LinuxError::from)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "perf_event_max_sample_rate",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", perf_event_max_sample_rate()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            set_perf_event_max_sample_rate(write_proc_u32(data)?)
                                .map_err(LinuxError::from)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            #[cfg(feature = "bpf")]
            kernel.add(
                "unprivileged_bpf_disabled",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", unprivileged_bpf_disabled()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            set_unprivileged_bpf_disabled(write_proc_u32(data)?)
                                .map_err(LinuxError::from)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add("keys", {
                let mut keys = DirMapping::new();
                keys.add(
                    "maxkeys",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                Ok(Some(alloc::format!("{}\n", key_maxkeys()).into_bytes()))
                            }
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_key_maxkeys(value).map_err(|_| VfsError::InvalidInput)?;
                                Ok(None)
                            }
                        }),
                    ),
                );
                keys.add(
                    "maxbytes",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                Ok(Some(alloc::format!("{}\n", key_maxbytes()).into_bytes()))
                            }
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_key_maxbytes(value).map_err(|_| VfsError::InvalidInput)?;
                                Ok(None)
                            }
                        }),
                    ),
                );
                keys.add(
                    "root_maxkeys",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => Ok(Some(
                                alloc::format!("{}\n", key_root_maxkeys()).into_bytes(),
                            )),
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_key_root_maxkeys(value).map_err(|_| VfsError::InvalidInput)?;
                                Ok(None)
                            }
                        }),
                    ),
                );
                keys.add(
                    "root_maxbytes",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => Ok(Some(
                                alloc::format!("{}\n", key_root_maxbytes()).into_bytes(),
                            )),
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_key_root_maxbytes(value).map_err(|_| VfsError::InvalidInput)?;
                                Ok(None)
                            }
                        }),
                    ),
                );
                SimpleDir::new_maker(fs.clone(), Arc::new(keys))
            });
            kernel.add(
                "shmall",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", shmall_limit()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            set_shmall_limit(write_proc_usize(data)?);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "msgmni",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", msgmni_limit()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_usize(data)?;
                            set_msgmni_limit(value);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "msg_next_id",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", msg_next_id()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_i32(data)?;
                            set_msg_next_id(value).map_err(|_| VfsError::InvalidInput)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "sem",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(sem_limits_string().into_bytes())),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let (semmsl, semmns, semopm, semmni) =
                                parse_sem_limits(data).ok_or(VfsError::InvalidInput)?;
                            set_sem_limits(semmsl, semmns, semopm, semmni);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "sem_next_id",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", sem_next_id()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_i32(data)?;
                            set_sem_next_id(value).map_err(|_| VfsError::InvalidInput)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "shmmax",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", shmmax_limit()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_usize(data)?;
                            set_shmmax_limit(value);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "shm_next_id",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", shm_next_id()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_i32(data)?;
                            set_shm_next_id(value).map_err(|_| VfsError::InvalidInput)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "shmmni",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", shmmni_limit()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            set_shmmni_limit(write_proc_usize(data)?)
                                .map_err(|_| VfsError::InvalidInput)?;
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add("random", {
                let mut random = DirMapping::new();
                random.add(
                    "entropy_avail",
                    SimpleFile::new_regular(fs.clone(), || {
                        Ok(format!("{}\n", crate::random::entropy_bits()))
                    }),
                );

                SimpleDir::new_maker(fs.clone(), Arc::new(random))
            });

            SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
        });

        sys.add("net", {
            let mut net = DirMapping::new();
            net.add("ipv4", {
                let mut ipv4 = DirMapping::new();
                ipv4.add("conf", {
                    let mut conf = DirMapping::new();
                    for iface in ["default", "lo"] {
                        conf.add(iface, {
                            let mut iface_dir = DirMapping::new();
                            iface_dir.add(
                                "tag",
                                SimpleFile::new_regular(fs.clone(), proc_ipv4_conf_tag_file(iface)),
                            );
                            SimpleDir::new_maker(fs.clone(), Arc::new(iface_dir))
                        });
                    }
                    SimpleDir::new_maker(fs.clone(), Arc::new(conf))
                });
                SimpleDir::new_maker(fs.clone(), Arc::new(ipv4))
            });
            SimpleDir::new_maker(fs.clone(), Arc::new(net))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });

    let proc_dir = ProcFsHandler(fs.clone());
    SimpleDir::new_maker(fs, Arc::new(proc_dir.chain(root)))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{format, string::String, vec};

    use super::*;

    fn kuid(raw: u32) -> Kuid {
        Kuid::from_raw(raw).unwrap()
    }

    fn kgid(raw: u32) -> Kgid {
        Kgid::from_raw(raw).unwrap()
    }

    #[test]
    fn oom_score_adj_accepts_in_range_raise_and_lower_values() {
        assert_eq!(validate_oom_score_adj_value(500), Ok(()));
        assert_eq!(validate_oom_score_adj_value(0), Ok(()));
        assert_eq!(parse_oom_score_adj_value(b"500\n"), Ok(500));
    }

    #[test]
    fn oom_score_adj_rejects_values_outside_linux_range() {
        assert_eq!(validate_oom_score_adj_value(-1000), Ok(()));
        assert_eq!(validate_oom_score_adj_value(1000), Ok(()));
        assert_eq!(
            validate_oom_score_adj_value(-1001),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            validate_oom_score_adj_value(1001),
            Err(VfsError::InvalidInput)
        );
    }

    #[test]
    fn timerslack_access_allows_self_without_capability() {
        assert_eq!(timerslack_access_decision(true, false), Ok(()));
    }

    #[test]
    fn timerslack_access_requires_capability_for_remote_task() {
        assert_eq!(timerslack_access_decision(false, true), Ok(()));
        assert_eq!(
            timerslack_access_decision(false, false),
            Err(VfsError::PermissionDenied)
        );
    }

    #[test]
    fn key_users_snapshot_maps_and_filters_for_reader_namespace() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();
        let map = child
            .try_build_uid_map(vec![IdMapInputExtent::new(0, 1000, 1)])
            .unwrap();
        child.publish_uid_map(map).unwrap();
        let record = |uid| KeyUserRecord {
            uid: kuid(uid),
            usage: 1,
            keys: 1,
            instantiated_keys: 1,
            quota_keys: 1,
            max_keys: 200,
            quota_bytes: 10,
            max_bytes: 20_000,
        };

        assert_eq!(
            render_key_users_snapshot(vec![record(1000), record(2000)], &child),
            "    0:     1 1/1 1/200 10/20000\n"
        );
    }

    #[test]
    fn id_map_parser_enforces_linux_record_bounds() {
        assert_eq!(
            parse_id_map_rows(b" 0 1000 1 \n").unwrap(),
            vec![IdMapInputExtent::new(0, 1000, 1)]
        );

        for invalid in [
            &b""[..],
            &b"0 1000"[..],
            &b"0 1000 1 junk\n"[..],
            &b"0 1000 1\n\n"[..],
            &b"0 1000 1\n \n"[..],
            &b"-1 1000 1\n"[..],
        ] {
            assert_eq!(parse_id_map_rows(invalid), Err(VfsError::InvalidInput));
        }

        let page = vec![b'0'; PAGE_SIZE_4K];
        assert_eq!(parse_id_map_rows(&page), Err(VfsError::InvalidInput));

        let mut too_many = String::new();
        for index in 0..=ID_MAP_MAX_EXTENTS {
            writeln!(&mut too_many, "{} {} 1", index * 2, index * 2).unwrap();
        }
        assert_eq!(
            parse_id_map_rows(too_many.as_bytes()),
            Err(VfsError::InvalidInput)
        );
    }

    #[test]
    fn user_namespace_control_parsers_preserve_linux_offset_and_spacing_rules() {
        assert_eq!(require_proc_userns_write_offset(0), Ok(()));
        assert_eq!(
            require_proc_userns_write_offset(1),
            Err(VfsError::InvalidInput)
        );

        assert_eq!(parse_setgroups_policy(b"allow"), Ok(true));
        assert_eq!(parse_setgroups_policy(b"deny\t\n"), Ok(false));
        for invalid in [
            &b" allow"[..],
            &b"deny!"[..],
            &b"deny\0"[..],
            "deny\u{2003}".as_bytes(),
            &b"allow   x"[..],
        ] {
            assert_eq!(parse_setgroups_policy(invalid), Err(VfsError::InvalidInput));
        }
    }

    #[test]
    fn id_map_render_uses_frozen_viewer_namespace() {
        let root = UserNamespace::try_new_root().unwrap();
        let parent = root.try_fork(kuid(1000), kgid(1000), true).unwrap();
        let parent_uid_map = parent
            .try_build_uid_map(vec![IdMapInputExtent::new(100, 1000, 20)])
            .unwrap();
        let parent_gid_map = parent
            .try_build_gid_map(vec![IdMapInputExtent::new(100, 1000, 20)])
            .unwrap();
        parent.publish_uid_map(parent_uid_map).unwrap();
        parent.publish_gid_map(parent_gid_map, false).unwrap();

        let child = parent.try_fork(kuid(1005), kgid(1005), false).unwrap();
        let child_uid_map = child
            .try_build_uid_map(vec![IdMapInputExtent::new(0, 105, 5)])
            .unwrap();
        child.publish_uid_map(child_uid_map).unwrap();

        assert_eq!(
            render_id_map(&child, &child, true).unwrap(),
            format!("{:>10} {:>10} {:>10}\n", 0, 105, 5).into_bytes()
        );
        assert_eq!(
            render_id_map(&child, &root, true).unwrap(),
            format!("{:>10} {:>10} {:>10}\n", 0, 1005, 5).into_bytes()
        );

        let unmapped_viewer = root.try_fork(kuid(2000), kgid(2000), false).unwrap();
        assert_eq!(
            render_id_map(&child, &unmapped_viewer, true).unwrap(),
            format!("{:>10} {:>10} {:>10}\n", 0, u32::MAX, 5).into_bytes()
        );
    }

    #[test]
    fn namespace_owner_proc_objects_map_to_explicit_owner() {
        let root = UserNamespace::try_new_root().unwrap();
        let child = root.try_fork(kuid(1000), kgid(1000), false).unwrap();

        let pid = PidNamespace::try_new_root(child.clone()).unwrap();
        let time = TimeNamespace::try_new_root(child.clone()).unwrap();
        let uts = UtsNamespace::try_new_root(child.clone()).unwrap();
        for object in [
            ProcNamespaceObject::Pid(pid),
            ProcNamespaceObject::Time(time),
            ProcNamespaceObject::Uts(uts),
        ] {
            let owner = object.owner_user_ns().unwrap();
            assert!(Arc::ptr_eq(&owner, &child));
        }

        let child_object = ProcNamespaceObject::User(child);
        assert!(Arc::ptr_eq(&child_object.owner_user_ns().unwrap(), &root));
        assert!(ProcNamespaceObject::User(root).owner_user_ns().is_none());
    }

    #[test]
    fn bpf_stats_are_read_only_and_keep_adapters_separate() {
        let output = String::from_utf8(render_proc_bpf_stats()).unwrap();
        assert!(output.starts_with("BPF_STATS schema=thekernel-bpf-stats-v1 "));
        assert!(output.contains(" seccomp_policy="));
        assert!(output.contains(" packet_policy="));
        assert!(output.contains("seccomp.published "));
        assert!(output.contains("seccomp.native_executed "));
        assert!(output.contains("seccomp.interpreter_executed "));
        assert!(output.contains("packet.published "));
        assert!(output.contains("packet.native_executed "));
        assert!(output.contains("packet.interpreter_executed "));
        assert!(output.contains("seccomp.fallback.translation "));
        assert!(output.contains("packet.fallback.translation "));
    }

    #[test]
    fn bpf_stats_open_snapshot_survives_chunked_read_and_concurrent_publication() {
        fn new_file() -> Arc<ProcBpfStatsFile> {
            let holder = Arc::new(Mutex::new(None));
            let holder_for_root = holder.clone();
            let filesystem = SimpleFs::new_with("proc-test".to_string(), 0, move |fs| {
                *holder_for_root.lock() = Some(ProcBpfStatsFile::try_new(fs.clone()).unwrap());
                SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
            });
            drop(filesystem);
            holder.lock().take().unwrap()
        }

        let file = new_file();
        file.open(true, false).unwrap();
        let length = file.len().unwrap() as usize;
        let mut expected = vec![0; length];
        file.read_at(&mut expected, 0).unwrap();

        let start = std::sync::Arc::new(std::sync::Barrier::new(2));
        let writer_start = start.clone();
        let writer = std::thread::spawn(move || {
            writer_start.wait();
            for _ in 0..256 {
                crate::seccomp_jit::try_reserve_published()
                    .expect("seccomp publication counter has capacity")
                    .commit();
                crate::packet_cbpf::try_reserve_published()
                    .expect("packet publication counter has capacity")
                    .commit();
                std::thread::yield_now();
            }
        });
        start.wait();

        let mut actual = Vec::with_capacity(length);
        let mut offset = 0;
        while offset < length {
            let chunk_len = (length - offset).min(7);
            let mut chunk = vec![0; chunk_len];
            assert_eq!(file.read_at(&mut chunk, offset as u64).unwrap(), chunk_len);
            actual.extend_from_slice(&chunk);
            offset += chunk_len;
            std::thread::yield_now();
        }
        writer.join().unwrap();

        assert_eq!(actual, expected);
    }

    #[cfg(feature = "test-io-control")]
    #[test]
    fn bpf_executor_control_parses_all_lines_before_publishing() {
        let old = crate::seccomp_jit::executor_policies();
        crate::seccomp_jit::set_executor_policies_for_control(
            Some(crate::seccomp_jit::ExecutorPolicy::Auto),
            Some(crate::seccomp_jit::ExecutorPolicy::Auto),
        );

        assert_eq!(
            parse_bpf_executor_control(b"seccomp=interpreter\npacket=jit\n"),
            Ok((
                Some(crate::seccomp_jit::ExecutorPolicy::Interpreter),
                Some(crate::seccomp_jit::ExecutorPolicy::Jit),
            ))
        );
        assert_eq!(
            parse_bpf_executor_control(b"seccomp=interpreter\npacket=bad\n"),
            Err(VfsError::InvalidInput)
        );
        assert_eq!(
            crate::seccomp_jit::executor_policies(),
            (
                crate::seccomp_jit::ExecutorPolicy::Auto,
                crate::seccomp_jit::ExecutorPolicy::Auto,
            )
        );

        crate::seccomp_jit::set_executor_policies_for_control(Some(old.0), Some(old.1));
    }

    #[cfg(feature = "test-io-control")]
    #[test]
    fn bpf_executor_control_readback_has_independent_domains() {
        let old = crate::seccomp_jit::executor_policies();
        crate::seccomp_jit::set_executor_policies_for_control(
            Some(crate::seccomp_jit::ExecutorPolicy::Interpreter),
            Some(crate::seccomp_jit::ExecutorPolicy::Jit),
        );
        assert_eq!(
            crate::seccomp_jit::executor_policy(),
            crate::seccomp_jit::ExecutorPolicy::Interpreter
        );
        assert_eq!(
            crate::seccomp_jit::packet_executor_policy(),
            crate::seccomp_jit::ExecutorPolicy::Jit
        );
        let output = String::from_utf8(render_proc_bpf_executor_control()).unwrap();
        assert!(output.contains("seccomp=interpreter\n"));
        assert!(output.contains("packet=jit\n"));
        crate::seccomp_jit::set_executor_policies_for_control(Some(old.0), Some(old.1));
    }
}
