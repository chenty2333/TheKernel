use alloc::{
    borrow::Cow,
    boxed::Box,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    ffi::CStr,
    fmt::Write as _,
    iter, str,
    sync::atomic::{AtomicI32, AtomicU32, AtomicUsize, Ordering},
    task::Context,
};

use axerrno::LinuxError;
use axfs::page_cache_pfn_is_dirty;
use axfs_ng_vfs::{
    DirEntry, FileNode, FileNodeOps, Filesystem, FilesystemOps, Location, Metadata, MetadataUpdate,
    NodeFlags, NodeOps, NodePermission, NodeType, Reference, VfsError, VfsResult,
};
use axhal::paging::MappingFlags;
use axpoll::{IoEvents, Pollable};
use axtask::{AxTaskRef, TaskState, WeakAxTaskRef, current, last_task_id, set_last_task_id};
use inherit_methods_macro::inherit_methods;
use linux_raw_sys::{
    general::{
        CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLONE_NEWPID, CLONE_NEWTIME, CLONE_NEWUSER, CLONE_NEWUTS,
        RLIM_INFINITY, RLIM_NLIMITS,
    },
    ioctl::{NS_GET_NSTYPE, NS_GET_OWNER_UID, NS_GET_PARENT, NS_GET_USERNS},
    mempolicy::{
        MPOL_BIND, MPOL_DEFAULT, MPOL_INTERLEAVE, MPOL_LOCAL, MPOL_PREFERRED, MPOL_PREFERRED_MANY,
    },
};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use starry_process::Process;
use starry_vm::VmMutPtr;

use crate::{
    file::{
        FD_TABLE, FileDescription, PidFd, fanotify::FanotifyFile, inotify::InotifyFile, lease, pipe,
    },
    mm::{
        Backend, BackendOps, commit_limit_bytes, committed_as_bytes, overcommit_memory_policy,
        overcommit_ratio, set_overcommit_memory_policy, set_overcommit_ratio, system_memory_stats,
    },
    mounts,
    pseudofs::{
        DirMaker, DirMapping, NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile,
        SimpleFileOperation, SimpleFs, SimpleFsNode,
        cgroup::{
            cpuset_allowed_masks, proc_cgroup_membership, proc_cgroups_snapshot,
            proc_cpuset_membership,
        },
        dev::RANDOM_ENTROPY_BITS,
    },
    syscall::{
        aio_max_nr, aio_nr, current_domainname_string, current_hostname_string,
        current_machine_string, current_release_string, current_sysname_string,
        current_version_string, key_gc_delay, key_maxbytes, key_maxkeys, key_root_maxbytes,
        key_root_maxkeys, key_users_snapshot, mq_msg_max, mq_msgsize_max, mq_queues_max,
        msg_next_id, msgmni_limit, parse_sem_limits, proc_version_string, sched_rr_timeslice_ms,
        sem_limits_string, sem_next_id, set_aio_max_nr, set_domainname_bytes, set_hostname_bytes,
        set_key_gc_delay, set_key_maxbytes, set_key_maxkeys, set_key_root_maxbytes,
        set_key_root_maxkeys, set_mq_msg_max, set_mq_msgsize_max, set_mq_queues_max,
        set_msg_next_id, set_msgmni_limit, set_sched_rr_timeslice_ms, set_sem_limits,
        set_sem_next_id, set_shm_next_id, set_shmmax_limit, shm_next_id, shmall_limit,
        shmmax_limit, shmmni_limit, swap_free_bytes, swap_snapshot, swap_total_bytes,
        sysvipc_msg_snapshot, sysvipc_sem_snapshot, sysvipc_shm_snapshot,
    },
    task::{
        AsThread, Mempolicy, PidNamespace, ProcessData, TimeNamespace, UserNamespace, UtsNamespace,
        get_process_data, get_process_including_zombie, get_task,
        get_visible_task_including_exiting, nr_open_limit, render_task_stat, render_zombie_stat,
        set_nr_open_limit, tasks,
    },
};

const PROC_PID_MAX_MIN: u32 = 301;
const PROC_PID_MAX_DEFAULT: u32 = 4_194_304;
const PROC_THREADS_MAX: u32 = 4_194_304;
const PROC_FILE_MAX_DEFAULT: usize = 1_048_576;
const PROC_SCHED_TIME_AVG_MS_DEFAULT: u32 = 1000;
const PROC_SCHED_RT_PERIOD_US_DEFAULT: u32 = 1_000_000;
const PROC_SCHED_RT_RUNTIME_US_DEFAULT: i32 = 950_000;
const PROC_PAGEMAP_ENTRY_BYTES: u64 = 8;
const PROC_KPAGEFLAGS_ENTRY_BYTES: u64 = 8;
const PROC_NUMA_NODEMASK: usize = 0b11;
const KPF_DIRTY: u64 = 1 << 4;
static PROC_PID_MAX: AtomicU32 = AtomicU32::new(PROC_PID_MAX_DEFAULT);
static PROC_FILE_MAX: AtomicUsize = AtomicUsize::new(PROC_FILE_MAX_DEFAULT);
static PROC_SCHED_TIME_AVG_MS: AtomicU32 = AtomicU32::new(PROC_SCHED_TIME_AVG_MS_DEFAULT);
static PROC_SCHED_RT_PERIOD_US: AtomicU32 = AtomicU32::new(PROC_SCHED_RT_PERIOD_US_DEFAULT);
static PROC_SCHED_RT_RUNTIME_US: AtomicI32 = AtomicI32::new(PROC_SCHED_RT_RUNTIME_US_DEFAULT);
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
// Minimal gzip-compressed kernel config for LTP kconfig probes.
const PROC_CONFIG_GZ: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x6d, 0xcc, 0xb1, 0x12, 0x40, 0x40,
    0x0c, 0x84, 0xe1, 0xde, 0x3b, 0x29, 0x58, 0x39, 0x32, 0xb8, 0x9c, 0x4b, 0x18, 0xaa, 0x3c, 0x87,
    0xb7, 0x37, 0x46, 0x91, 0x46, 0xb9, 0xdf, 0x3f, 0xb3, 0x90, 0x9c, 0x78, 0x74, 0xbd, 0xf4, 0xe0,
    0x82, 0xf6, 0x6e, 0xf0, 0x01, 0x26, 0xc2, 0x5c, 0x84, 0xb3, 0x79, 0x25, 0x35, 0xa9, 0x14, 0xad,
    0x63, 0x89, 0x51, 0x44, 0xf9, 0xf4, 0x75, 0xdb, 0x69, 0xa7, 0x7f, 0x7d, 0xcf, 0x61, 0x4b, 0xc4,
    0x5e, 0x07, 0x2f, 0x55, 0x40, 0xaa, 0xde, 0x01, 0x16, 0x65, 0xc8, 0x62, 0x9c, 0xae, 0x00, 0x5b,
    0xb4, 0xbd, 0x9b, 0x07, 0xf4, 0x50, 0x9d, 0x4d, 0xa5, 0x00, 0x00, 0x00,
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
        "cgroup" if !matches!(record.source.as_str(), "none" | "cgroup") => {
            Some(record.source.as_str())
        }
        "cgroup2" if !record.data.is_empty() => Some(record.data.as_str()),
        _ => None,
    };
    if let Some(data) = data {
        append_mount_data_options(&mut options, data);
    }
    options
}

fn render_mounts() -> String {
    let mut out = String::from("proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n");
    for record in mounts::snapshot() {
        let options = record_mount_options(&record);
        let _ = writeln!(
            out,
            "{} {} {} {} 0 0",
            record.source, record.target, record.fs_type, options
        );
    }
    out
}

fn render_mountinfo() -> String {
    let mut out = String::from("1 0 0:1 / / rw,relatime - rootfs rootfs rw\n");
    for record in mounts::snapshot() {
        let major = record.dev >> 32;
        let minor = record.dev as u32;
        let options = record_mount_options(&record);
        let _ = writeln!(
            out,
            "{} 1 {}:{} / {} {} - {} {} {}",
            record.dev,
            major,
            minor,
            record.target,
            options,
            record.fs_type,
            record.source,
            options
        );
    }
    out
}

fn proc_task_for_pid(pid: u32) -> VfsResult<AxTaskRef> {
    if let Ok(task) = get_visible_task_including_exiting(pid) {
        return Ok(task);
    }

    let proc_data = get_process_data(pid).map_err(|_| VfsError::NotFound)?;
    for tid in proc_data.proc.threads() {
        if let Ok(task) = get_task(tid)
            && !task.as_thread().pending_exit()
        {
            return Ok(task);
        }
    }

    Err(VfsError::NotFound)
}

fn real_meminfo() -> String {
    let stats = system_memory_stats();
    let total_kb = stats.total_bytes / 1024;
    let free_kb = stats.free_bytes / 1024;
    let available_kb = stats.available_bytes / 1024;
    let used_kb = stats.used_bytes / 1024;
    let cached_kb = stats.cached_bytes / 1024;
    let mapped_kb = stats.mapped_bytes / 1024;
    let page_tables_kb = stats.page_table_bytes / 1024;
    let swap_total_kb = swap_total_bytes() as usize / 1024;
    let swap_free_kb = swap_free_bytes() as usize / 1024;
    let commit_limit_kb = commit_limit_bytes() / 1024;
    let committed_kb = committed_as_bytes() / 1024;
    format!(
        "MemTotal:       {total_kb:>8} kB\n\
         MemFree:        {free_kb:>8} kB\n\
         MemAvailable:   {available_kb:>8} kB\n\
         Buffers:               0 kB\n\
         Cached:         {cached_kb:>8} kB\n\
         SwapCached:            0 kB\n\
         Active:         {used_kb:>8} kB\n\
         Inactive:              0 kB\n\
         SwapTotal:      {swap_total_kb:>8} kB\n\
         SwapFree:       {swap_free_kb:>8} kB\n\
         Dirty:                 0 kB\n\
         Writeback:             0 kB\n\
         AnonPages:             0 kB\n\
         Mapped:         {mapped_kb:>8} kB\n\
         Shmem:                 0 kB\n\
         Slab:                  0 kB\n\
         PageTables:     {page_tables_kb:>8} kB\n\
         CommitLimit:    {commit_limit_kb:>8} kB\n\
         Committed_AS:   {committed_kb:>8} kB\n\
         VmallocTotal:          0 kB\n\
         VmallocUsed:           0 kB\n"
    )
}

fn rw_static_file(content: &'static str) -> impl crate::pseudofs::SimpleFileOps {
    RwFile::new(move |req| match req {
        SimpleFileOperation::Read => Ok(Some(content.as_bytes().to_vec())),
        SimpleFileOperation::Write(_) => Ok(None),
    })
}

fn current_net_ipv4_conf_tag(iface: &str) -> VfsResult<i32> {
    current()
        .as_thread()
        .proc_data
        .net_ns
        .ipv4_conf_tag(iface)
        .ok_or(VfsError::NotFound)
}

fn set_current_net_ipv4_conf_tag(iface: &str, value: i32) -> VfsResult<()> {
    current()
        .as_thread()
        .proc_data
        .net_ns
        .set_ipv4_conf_tag(iface, value)
        .map_err(|_| VfsError::NotFound)
}

fn proc_ipv4_conf_tag_file(iface: &'static str) -> impl crate::pseudofs::SimpleFileOps {
    RwFile::new(move |req| match req {
        SimpleFileOperation::Read => Ok(Some(
            format!("{}\n", current_net_ipv4_conf_tag(iface)?).into_bytes(),
        )),
        SimpleFileOperation::Write(data) => {
            if data.iter().all(|byte| byte.is_ascii_whitespace()) {
                return Ok(None);
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
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        let Some(process) = self.process.upgrade() else {
            return Box::new(iter::empty());
        };
        Box::new(process.threads().into_iter().filter_map(|tid| {
            let task = get_task(tid).ok()?;
            Some(Cow::Owned(task.as_thread().tid().to_string()))
        }))
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let process = self.process.upgrade().ok_or(VfsError::NotFound)?;
        let tid = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let task = proc_task_for_pid(tid)?;
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
fn task_status(task: &AxTaskRef) -> String {
    let proc_data = &task.as_thread().proc_data;
    let locked_kb = {
        let aspace_handle = proc_data.aspace();
        let aspace = aspace_handle.lock();
        aspace.locked_bytes() / 1024
    };
    let caps = proc_data.capability_state();
    let cpu_mask = task_cpu_mask_bits(task);
    let mem_mask = cpuset_allowed_masks(proc_data.proc.pid())
        .map(|(_, mems)| mems)
        .unwrap_or(PROC_NUMA_NODEMASK);
    let cpu_width = axhal::cpu_num().max(1);
    let mem_width = PROC_NUMA_NODEMASK
        .next_power_of_two()
        .trailing_zeros()
        .max(1) as usize;
    let cpu_allowed_list = format_mask_list(cpu_mask, cpu_width);
    let mem_allowed_list = format_mask_list(mem_mask, mem_width);
    format!(
        "Tgid:\t{}\n\
        Pid:\t{}\n\
        Uid:\t0 0 0 0\n\
        Gid:\t0 0 0 0\n\
        VmLck:\t{} kB\n\
        VmSwap:\t0 kB\n\
        NoNewPrivs:\t{}\n\
        CapInh:\t{}\n\
        CapPrm:\t{}\n\
        CapEff:\t{}\n\
        CapBnd:\t{}\n\
        CapAmb:\t{}\n\
        Cpus_allowed:\t{:x}\n\
        Cpus_allowed_list:\t{}\n\
        Mems_allowed:\t{:x}\n\
        Mems_allowed_list:\t{}",
        proc_data.proc.pid(),
        task.as_thread().tid(),
        locked_kb,
        proc_data.no_new_privs() as u8,
        format_cap_set(caps.inheritable),
        format_cap_set(caps.permitted),
        format_cap_set(caps.effective),
        format_cap_set(caps.bounding),
        format_cap_set(caps.ambient),
        cpu_mask,
        cpu_allowed_list,
        mem_mask,
        mem_allowed_list
    )
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

fn render_task_maps(task: &AxTaskRef, include_smaps: bool) -> String {
    let thr = task.as_thread();
    let aspace_handle = thr.proc_data.aspace();
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

fn render_task_numa_maps(task: &AxTaskRef) -> String {
    let thr = task.as_thread();
    let proc_data = &thr.proc_data;
    let aspace_handle = proc_data.aspace();
    let aspace = aspace_handle.lock();
    let mut out = String::new();

    for area in aspace.areas() {
        if !area.flags().contains(MappingFlags::USER) {
            continue;
        }
        let start = area.start().as_usize();
        let end = start + area.size();
        let policy = proc_data
            .mempolicy_for_addr(start)
            .unwrap_or_else(|| proc_data.mempolicy());
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
}

impl SimpleDirOps for ThreadFdDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        let Some(task) = self.task.upgrade() else {
            return Box::new(iter::empty());
        };
        let ids = FD_TABLE
            .scope(&task.as_thread().proc_data.scope.read())
            .read()
            .ids()
            .map(|id| Cow::Owned(id.to_string()))
            .collect::<Vec<_>>();
        Box::new(ids.into_iter())
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let fd = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let path = FD_TABLE
            .scope(&task.as_thread().proc_data.scope.read())
            .read()
            .get(fd as _)
            .ok_or(VfsError::NotFound)?
            .description
            .inner
            .path()
            .into_owned();
        Ok(SimpleFile::new(fs, NodeType::Symlink, move || Ok(path.clone())).into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

/// The /proc/[pid]/fdinfo directory
struct ThreadFdInfoDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
}

impl ThreadFdInfoDir {
    fn description_for(&self, name: &str) -> VfsResult<Arc<FileDescription>> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let fd = name.parse::<usize>().map_err(|_| VfsError::NotFound)?;
        FD_TABLE
            .scope(&task.as_thread().proc_data.scope.read())
            .read()
            .get(fd)
            .map(|entry| entry.description.clone())
            .ok_or(VfsError::NotFound)
    }

    fn render_fdinfo(description: &FileDescription) -> String {
        let stat = description.inner.stat().ok();
        let mnt_id = stat.map_or(0, |stat| stat.dev);
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
        if let Some(pidfd) = description.inner.downcast_ref::<PidFd>() {
            if let Ok(proc_data) = pidfd.process_data() {
                let pid = proc_data.proc.pid();
                let _ = writeln!(out, "Pid:\t{pid}");
                let _ = writeln!(out, "NSpid:\t{pid}");
            }
        }
        out
    }
}

impl SimpleDirOps for ThreadFdInfoDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        let Some(task) = self.task.upgrade() else {
            return Box::new(iter::empty());
        };
        let ids = FD_TABLE
            .scope(&task.as_thread().proc_data.scope.read())
            .read()
            .ids()
            .map(|id| Cow::Owned(id.to_string()))
            .collect::<Vec<_>>();
        Box::new(ids.into_iter())
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
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
    Pid,
    Time,
    TimeForChildren,
    User,
    Uts,
}

pub(crate) enum ProcNamespaceObject {
    Pid(Arc<PidNamespace>),
    Time(Arc<TimeNamespace>),
    User(Arc<UserNamespace>),
    Uts(Arc<UtsNamespace>),
}

struct ProcNamespaceFile {
    node: SimpleFsNode,
    fs: Arc<SimpleFs>,
    kind: ProcNamespaceKind,
    object: ProcNamespaceObject,
}

impl ProcNamespaceFile {
    fn new(fs: Arc<SimpleFs>, kind: ProcNamespaceKind, task: &AxTaskRef) -> Arc<Self> {
        let proc_data = &task.as_thread().proc_data;
        let object = match kind {
            ProcNamespaceKind::Pid => ProcNamespaceObject::Pid(proc_data.pid_ns()),
            ProcNamespaceKind::Time => ProcNamespaceObject::Time(proc_data.time_ns()),
            ProcNamespaceKind::TimeForChildren => {
                ProcNamespaceObject::Time(proc_data.time_ns_for_children())
            }
            ProcNamespaceKind::User => ProcNamespaceObject::User(proc_data.user_ns()),
            ProcNamespaceKind::Uts => ProcNamespaceObject::Uts(proc_data.uts_ns()),
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

    fn nstype(&self) -> u32 {
        match self.kind {
            ProcNamespaceKind::Pid => CLONE_NEWPID,
            ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren => CLONE_NEWTIME,
            ProcNamespaceKind::User => CLONE_NEWUSER,
            ProcNamespaceKind::Uts => CLONE_NEWUTS,
        }
    }

    fn namespace_inode(&self) -> Option<u64> {
        match &self.object {
            ProcNamespaceObject::Pid(ns) => Some(ns.proc_inode()),
            ProcNamespaceObject::User(ns) => Some(ns.proc_inode()),
            ProcNamespaceObject::Time(_) | ProcNamespaceObject::Uts(_) => None,
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
        NodeFlags::NON_CACHEABLE
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

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            NS_GET_PARENT => match self.kind {
                ProcNamespaceKind::Pid => Err(VfsError::OperationNotPermitted),
                ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren => {
                    Err(VfsError::InvalidInput)
                }
                ProcNamespaceKind::User => Err(VfsError::InvalidInput),
                ProcNamespaceKind::Uts => Err(VfsError::InvalidInput),
            },
            NS_GET_USERNS => match self.kind {
                ProcNamespaceKind::Pid
                | ProcNamespaceKind::Time
                | ProcNamespaceKind::TimeForChildren
                | ProcNamespaceKind::User
                | ProcNamespaceKind::Uts => Err(VfsError::OperationNotPermitted),
            },
            NS_GET_OWNER_UID => match &self.object {
                ProcNamespaceObject::User(ns) => {
                    (arg as *mut u32).vm_write(ns.owner_uid())?;
                    Ok(0)
                }
                _ => Err(VfsError::InvalidInput),
            },
            NS_GET_NSTYPE => Ok(self.nstype() as usize),
            _ => Err(VfsError::NotATty),
        }
    }
}

impl Pollable for ProcNamespaceFile {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

struct ThreadNamespaceDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
}

impl SimpleDirOps for ThreadNamespaceDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        if self.task.upgrade().is_none() {
            return Box::new(iter::empty());
        }
        Box::new(
            ["pid", "time", "time_for_children", "user", "uts"]
                .into_iter()
                .map(Cow::Borrowed),
        )
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let kind = match name {
            "pid" => ProcNamespaceKind::Pid,
            "time" => ProcNamespaceKind::Time,
            "time_for_children" => ProcNamespaceKind::TimeForChildren,
            "user" => ProcNamespaceKind::User,
            "uts" => ProcNamespaceKind::Uts,
            _ => return Err(VfsError::NotFound),
        };
        Ok(ProcNamespaceFile::new(self.fs.clone(), kind, &task).into())
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
        ProcNamespaceObject::Pid(ns) => ProcNamespaceObject::Pid(ns.clone()),
        ProcNamespaceObject::Time(ns) => ProcNamespaceObject::Time(ns.clone()),
        ProcNamespaceObject::User(ns) => ProcNamespaceObject::User(ns.clone()),
        ProcNamespaceObject::Uts(ns) => ProcNamespaceObject::Uts(ns.clone()),
    };
    ProcNamespaceTarget::Live(file.kind, object)
}

pub(crate) fn proc_namespace_location_from_object(
    template: &Location,
    kind: ProcNamespaceKind,
    object: ProcNamespaceObject,
) -> VfsResult<Location> {
    let parent = template.entry().parent();
    let name = match kind {
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
        Reference::new(parent, name.into()),
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
    task.as_thread()
        .proc_data
        .time_ns_for_children()
        .render_offsets()
}

fn write_timens_offsets(task: &AxTaskRef, data: &[u8]) -> VfsResult<()> {
    if data.len() >= PAGE_SIZE_4K {
        return Err(VfsError::InvalidInput);
    }
    let text = str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let proc_data = &task.as_thread().proc_data;
    if !proc_data.has_effective_capability(linux_raw_sys::general::CAP_SYS_TIME) {
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

    let time_ns = proc_data.time_ns_for_children();
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
    task: WeakAxTaskRef,
}

impl ProcPagemapFile {
    fn new(fs: Arc<SimpleFs>, task: WeakAxTaskRef) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o444),
            ),
            task,
        })
    }

    fn pagemap_entry(&self, vpn: u64) -> u64 {
        let Some(task) = self.task.upgrade() else {
            return 0;
        };
        let Some(vaddr) = vpn
            .checked_mul(PAGE_SIZE_4K as u64)
            .and_then(|addr| usize::try_from(addr).ok())
            .map(VirtAddr::from)
        else {
            return 0;
        };
        let aspace_handle = task.as_thread().proc_data.aspace();
        let aspace = aspace_handle.lock();
        match aspace.page_table().query(vaddr) {
            Ok((paddr, ..)) => (1u64 << 63) | (paddr.as_usize() as u64 / PAGE_SIZE_4K as u64),
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

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

impl Pollable for ProcPagemapFile {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

struct ProcKpageflagsFile {
    node: SimpleFsNode,
}

impl ProcKpageflagsFile {
    fn new(fs: Arc<SimpleFs>) -> Arc<Self> {
        Arc::new(Self {
            node: SimpleFsNode::new(
                fs,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o444),
            ),
        })
    }

    fn kpageflags_entry(&self, pfn: u64) -> u64 {
        usize::try_from(pfn)
            .ok()
            .filter(|pfn| page_cache_pfn_is_dirty(*pfn))
            .map_or(0, |_| KPF_DIRTY)
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for ProcKpageflagsFile {
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

impl FileNodeOps for ProcKpageflagsFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let mut written = 0;
        let mut entry_index = offset / PROC_KPAGEFLAGS_ENTRY_BYTES;
        let mut entry_offset = (offset % PROC_KPAGEFLAGS_ENTRY_BYTES) as usize;

        while written < buf.len() {
            let entry = self.kpageflags_entry(entry_index).to_le_bytes();
            let copy_len =
                (PROC_KPAGEFLAGS_ENTRY_BYTES as usize - entry_offset).min(buf.len() - written);
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

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

impl Pollable for ProcKpageflagsFile {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

impl SimpleDirOps for ZombieProcessDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        if self.process.upgrade().is_some() {
            Box::new(iter::once(Cow::Borrowed("stat")))
        } else {
            Box::new(iter::empty())
        }
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        if name != "stat" {
            return Err(VfsError::NotFound);
        }
        let fs = self.fs.clone();
        let process = self.process.upgrade().ok_or(VfsError::NotFound)?;
        Ok(
            SimpleFile::new_regular(fs, move || Ok(render_zombie_stat(&process)?.into_bytes()))
                .into(),
        )
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

impl SimpleDirOps for ThreadDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            [
                Some("stat"),
                Some("status"),
                Some("limits"),
                Some("oom_score_adj"),
                Some("cgroup"),
                Some("cpuset"),
                self.show_task_dir.then_some("task"),
                Some("maps"),
                Some("smaps"),
                Some("numa_maps"),
                Some("pagemap"),
                Some("mounts"),
                Some("mountinfo"),
                Some("cmdline"),
                Some("coredump_filter"),
                Some("timerslack_ns"),
                Some("timens_offsets"),
                Some("comm"),
                Some("exe"),
                Some("fd"),
                Some("fdinfo"),
                Some("ns"),
            ]
            .into_iter()
            .flatten()
            .map(Cow::Borrowed),
        )
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        Ok(match name {
            "stat" => {
                SimpleFile::new_regular(fs, move || Ok(render_task_stat(&task)?.into_bytes()))
                    .into()
            }
            "status" => SimpleFile::new_regular(fs, move || Ok(task_status(&task))).into(),
            "limits" => SimpleFile::new_regular(fs, move || Ok(render_task_limits(&task))).into(),
            "oom_score_adj" => SimpleFile::new_regular(
                fs,
                RwFile::new(move |req| match req {
                    SimpleFileOperation::Read => Ok(Some(
                        task.as_thread().oom_score_adj().to_string().into_bytes(),
                    )),
                    SimpleFileOperation::Write(data) => {
                        if !data.is_empty() {
                            let value = str::from_utf8(data)
                                .ok()
                                .and_then(|it| it.parse::<i32>().ok())
                                .ok_or(VfsError::InvalidInput)?;
                            task.as_thread().set_oom_score_adj(value);
                        }
                        Ok(None)
                    }
                }),
            )
            .into(),
            "cgroup" => {
                let pid = task.as_thread().proc_data.proc.pid();
                SimpleFile::new_regular(fs, move || Ok(proc_cgroup_membership(pid))).into()
            }
            "cpuset" => {
                let pid = task.as_thread().proc_data.proc.pid();
                SimpleFile::new_regular(fs, move || Ok(proc_cpuset_membership(pid))).into()
            }
            "task" if self.show_task_dir => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ProcessTaskDir {
                    fs,
                    process: Arc::downgrade(&task.as_thread().proc_data.proc),
                }),
            )
            .into(),
            "maps" => {
                SimpleFile::new_regular(fs, move || Ok(render_task_maps(&task, false))).into()
            }
            "smaps" => {
                SimpleFile::new_regular(fs, move || Ok(render_task_maps(&task, true))).into()
            }
            "numa_maps" => {
                SimpleFile::new_regular(fs, move || Ok(render_task_numa_maps(&task))).into()
            }
            "pagemap" => ProcPagemapFile::new(fs, Arc::downgrade(&task)).into(),
            "mounts" => SimpleFile::new_regular(fs, move || Ok(render_mounts())).into(),
            "mountinfo" => SimpleFile::new_regular(fs, move || Ok(render_mountinfo())).into(),
            "cmdline" => SimpleFile::new_regular(fs, move || {
                let cmdline = task.as_thread().proc_data.cmdline.read();
                let mut buf = Vec::new();
                for arg in cmdline.iter() {
                    buf.extend_from_slice(arg.as_bytes());
                    buf.push(0);
                }
                Ok(buf)
            })
            .into(),
            "coredump_filter" => SimpleFile::new_regular(fs, rw_static_file("33\n")).into(),
            "timerslack_ns" => SimpleFile::new_regular(
                fs,
                RwFile::new(move |req| match req {
                    SimpleFileOperation::Read => Ok(Some(
                        format!("{}\n", task.as_thread().proc_data.timerslack_ns()).into_bytes(),
                    )),
                    SimpleFileOperation::Write(data) => {
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
            )
            .into(),
            "timens_offsets" => SimpleFile::new_regular(
                fs,
                RwFile::new(move |req| match req {
                    SimpleFileOperation::Read => Ok(Some(render_timens_offsets(&task))),
                    SimpleFileOperation::Write(data) => {
                        write_timens_offsets(&task, data)?;
                        Ok(None)
                    }
                }),
            )
            .into(),
            "comm" => SimpleFile::new_regular(
                fs,
                RwFile::new(move |req| match req {
                    SimpleFileOperation::Read => {
                        let name = task.name();
                        let copy_len = name.len().min(15);
                        let mut bytes = Vec::with_capacity(copy_len + 1);
                        bytes.extend_from_slice(&name.as_bytes()[..copy_len]);
                        bytes.push(b'\n');
                        Ok(Some(bytes))
                    }
                    SimpleFileOperation::Write(data) => {
                        if !data.is_empty() {
                            let mut input = [0; 16];
                            let data = data.strip_suffix(b"\n").unwrap_or(data);
                            let copy_len = data.len().min(15);
                            input[..copy_len].copy_from_slice(&data[..copy_len]);
                            task.set_name(
                                CStr::from_bytes_until_nul(&input)
                                    .map_err(|_| VfsError::InvalidInput)?
                                    .to_str()
                                    .map_err(|_| VfsError::InvalidInput)?,
                            );
                        }
                        Ok(None)
                    }
                }),
            )
            .into(),
            "exe" => SimpleFile::new(fs, NodeType::Symlink, move || {
                Ok(task.as_thread().proc_data.exe_path.read().clone())
            })
            .into(),
            "fd" => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ThreadFdDir {
                    fs,
                    task: Arc::downgrade(&task),
                }),
            )
            .into(),
            "fdinfo" => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ThreadFdInfoDir {
                    fs,
                    task: Arc::downgrade(&task),
                }),
            )
            .into(),
            "ns" => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ThreadNamespaceDir {
                    fs,
                    task: Arc::downgrade(&task),
                }),
            )
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
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            tasks()
                .into_iter()
                .filter(|task| !task.as_thread().pending_exit())
                .map(|task| task.as_thread().tid().to_string().into())
                .chain([Cow::Borrowed("self")]),
        )
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        if name == "self" {
            let task = current().clone();
            return Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
                self.0.clone(),
                Arc::new(ThreadDir {
                    fs: self.0.clone(),
                    task: Arc::downgrade(&task),
                    show_task_dir: true,
                }),
            )));
        }

        let pid = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
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
        if !process.is_zombie() || process.zombie_snapshot().is_none() {
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

    fn write_proc_long_max_bounded_usize(data: &[u8]) -> VfsResult<usize> {
        let value = str::from_utf8(data)
            .ok()
            .map(str::trim)
            .and_then(|it| it.parse::<u64>().ok())
            .ok_or(VfsError::InvalidInput)?;
        if value > i64::MAX as u64 {
            return Err(VfsError::InvalidInput);
        }
        Ok(value as usize)
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
    root.add(
        "mounts",
        SimpleFile::new_regular(fs.clone(), || Ok(render_mounts())),
    );
    root.add(
        "mountinfo",
        SimpleFile::new_regular(fs.clone(), || Ok(render_mountinfo())),
    );
    root.add("sysvipc", {
        let mut sysvipc = DirMapping::new();
        sysvipc.add(
            "msg",
            SimpleFile::new_regular(fs.clone(), || Ok(sysvipc_msg_snapshot())),
        );
        sysvipc.add(
            "shm",
            SimpleFile::new_regular(fs.clone(), || Ok(sysvipc_shm_snapshot())),
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
        "cgroups",
        SimpleFile::new_regular(fs.clone(), || Ok(proc_cgroups_snapshot())),
    );
    root.add(
        "swaps",
        SimpleFile::new_regular(fs.clone(), || Ok(swap_snapshot())),
    );
    root.add(
        "config.gz",
        SimpleFile::new_regular(fs.clone(), || Ok(PROC_CONFIG_GZ.to_vec())),
    );
    root.add(
        "meminfo2",
        SimpleFile::new_regular(fs.clone(), || {
            let allocator = axalloc::global_allocator();
            Ok(format!("{:?}\n", allocator.usages()))
        }),
    );
    root.add("kpageflags", ProcKpageflagsFile::new(fs.clone()));
    root.add(
        "instret",
        SimpleFile::new_regular(fs.clone(), || {
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            {
                Ok(format!("{}\n", riscv::register::instret::read64()))
            }
            #[cfg(not(any(target_arch = "riscv32", target_arch = "riscv64")))]
            {
                Ok("0\n".to_string())
            }
        }),
    );
    {
        static IRQ_CNT: AtomicUsize = AtomicUsize::new(0);

        axtask::register_timer_callback(|_| {
            IRQ_CNT.fetch_add(1, Ordering::Relaxed);
        });

        root.add(
            "interrupts",
            SimpleFile::new_regular(fs.clone(), || {
                Ok(format!("0: {}", IRQ_CNT.load(Ordering::Relaxed)))
            }),
        );
    }

    root.add(
        "cpuinfo",
        SimpleFile::new_regular(fs.clone(), || {
            let num_cpus = axhal::cpu_num();
            let mut out = String::new();
            for i in 0..num_cpus {
                if i > 0 {
                    out.push('\n');
                }
                #[cfg(target_arch = "riscv64")]
                {
                    let _ = write!(
                        out,
                        "processor\t: {i}\nhart\t\t: {i}\nisa\t\t: rv64imafdc\nmmu\t\t: sv39\n"
                    );
                }
                #[cfg(target_arch = "aarch64")]
                {
                    let _ = write!(
                        out,
                        "processor\t: {i}\nBogoMIPS\t: 48.00\nFeatures\t: fp asimd\n"
                    );
                }
                #[cfg(target_arch = "x86_64")]
                {
                    let _ = write!(
                        out,
                        "processor\t: {i}\nvendor_id\t: GenuineIntel\nmodel name\t: QEMU Virtual \
                         CPU\n"
                    );
                }
                #[cfg(target_arch = "loongarch64")]
                {
                    let _ = write!(out, "processor\t: {i}\nISA\t\t: loongarch64\n");
                }
            }
            Ok(out)
        }),
    );
    root.add(
        "key-users",
        SimpleFile::new_regular(fs.clone(), || Ok(key_users_snapshot())),
    );
    root.add(
        "version",
        SimpleFile::new_regular(fs.clone(), || Ok(proc_version_string())),
    );
    root.add(
        "uptime",
        SimpleFile::new_regular(fs.clone(), || {
            let uptime = current()
                .as_thread()
                .proc_data
                .time_ns()
                .apply_boottime_offset(axhal::time::monotonic_time());
            let secs = uptime.as_secs();
            let centisecs = uptime.subsec_nanos() / 10_000_000;
            Ok(format!("{secs}.{centisecs:02} 0.00\n"))
        }),
    );
    root.add(
        "loadavg",
        SimpleFile::new_regular(fs.clone(), || {
            let all_tasks = tasks()
                .into_iter()
                .filter(|task| !task.as_thread().pending_exit())
                .collect::<Vec<_>>();
            let total = all_tasks.len();
            let running = all_tasks
                .iter()
                .filter(|t| matches!(t.state(), TaskState::Running | TaskState::Ready))
                .count();
            // Approximate load as running/total ratio, clamped.
            let load = running as f64;
            let last_pid = all_tasks
                .iter()
                .map(|t| t.as_thread().tid() as u64)
                .max()
                .unwrap_or(0);
            Ok(format!(
                "{load:.2} {load:.2} {load:.2} {running}/{total} {last_pid}\n"
            ))
        }),
    );
    root.add(
        "cmdline",
        SimpleFile::new_regular(fs.clone(), || Ok("console=ttyS0\n")),
    );

    root.add("sys", {
        let mut sys = DirMapping::new();

        sys.add("fs", {
            let mut fs_dir = DirMapping::new();

            fs_dir.add(
                "file-max",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", PROC_FILE_MAX.load(Ordering::Relaxed))
                                .into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_long_max_bounded_usize(data)?;
                            PROC_FILE_MAX.store(value, Ordering::Relaxed);
                            Ok(None)
                        }
                    }),
                ),
            );
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
                            if value == 0 {
                                return Err(VfsError::InvalidInput);
                            }
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
            fs_dir.add(
                "pipe-user-pages-soft",
                SimpleFile::new_regular(fs.clone(), rw_static_file("16\n")),
            );
            fs_dir.add("inotify", {
                let mut inotify = DirMapping::new();
                inotify.add(
                    "max_queued_events",
                    SimpleFile::new_regular(fs.clone(), rw_static_file("16384\n")),
                );
                inotify.add(
                    "max_user_instances",
                    SimpleFile::new_regular(fs.clone(), rw_static_file("1024\n")),
                );
                inotify.add(
                    "max_user_watches",
                    SimpleFile::new_regular(fs.clone(), rw_static_file("1048576\n")),
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
                    || Ok(format!("{}\n", current_machine_string()).into_bytes()),
                ),
            );
            kernel.add(
                "ostype",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o444),
                    || Ok(format!("{}\n", current_sysname_string()).into_bytes()),
                ),
            );
            kernel.add(
                "osrelease",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o444),
                    || Ok(format!("{}\n", current_release_string()).into_bytes()),
                ),
            );
            kernel.add(
                "version",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o444),
                    || Ok(format!("{}\n", current_version_string()).into_bytes()),
                ),
            );
            kernel.add(
                "domainname",
                SimpleFile::new_regular_with_permission(
                    fs.clone(),
                    NodePermission::from_bits_truncate(0o644),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            format!("{}\n", current_domainname_string()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if let Some(value) = proc_uts_write_value(data) {
                                set_domainname_bytes(value);
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
                        SimpleFileOperation::Read => Ok(Some(
                            format!("{}\n", current_hostname_string()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if let Some(value) = proc_uts_write_value(data) {
                                set_hostname_bytes(value);
                            }
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "pid_max",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", PROC_PID_MAX.load(Ordering::Relaxed))
                                .into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_u32(data)?;
                            if !(PROC_PID_MAX_MIN..=PROC_PID_MAX_DEFAULT).contains(&value) {
                                return Err(VfsError::InvalidInput);
                            }
                            PROC_PID_MAX.store(value, Ordering::Relaxed);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "ns_last_pid",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => {
                            Ok(Some(alloc::format!("{}\n", last_task_id()).into_bytes()))
                        }
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_u32(data)?;
                            if value >= PROC_PID_MAX.load(Ordering::Relaxed) {
                                return Err(VfsError::InvalidInput);
                            }
                            set_last_task_id(value.into());
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "threads-max",
                SimpleFile::new_regular(fs.clone(), || Ok(format!("{PROC_THREADS_MAX}\n"))),
            );
            kernel.add(
                "sched_time_avg_ms",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", PROC_SCHED_TIME_AVG_MS.load(Ordering::Relaxed))
                                .into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_u32(data)?;
                            if value == 0 {
                                return Err(VfsError::InvalidInput);
                            }
                            PROC_SCHED_TIME_AVG_MS.store(value, Ordering::Relaxed);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "sched_rt_period_us",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!("{}\n", PROC_SCHED_RT_PERIOD_US.load(Ordering::Relaxed))
                                .into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_i32(data)?;
                            if value <= 0 {
                                return Err(VfsError::InvalidInput);
                            }
                            let runtime = PROC_SCHED_RT_RUNTIME_US.load(Ordering::Relaxed);
                            if runtime > value {
                                return Err(VfsError::InvalidInput);
                            }
                            PROC_SCHED_RT_PERIOD_US.store(value as u32, Ordering::Relaxed);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "sched_rt_runtime_us",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            alloc::format!(
                                "{}\n",
                                PROC_SCHED_RT_RUNTIME_US.load(Ordering::Relaxed)
                            )
                            .into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let value = write_proc_i32(data)?;
                            if value < -1
                                || value > PROC_SCHED_RT_PERIOD_US.load(Ordering::Relaxed) as i32
                            {
                                return Err(VfsError::InvalidInput);
                            }
                            PROC_SCHED_RT_RUNTIME_US.store(value, Ordering::Relaxed);
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
            kernel.add("keys", {
                let mut keys = DirMapping::new();
                keys.add(
                    "gc_delay",
                    SimpleFile::new_regular(
                        fs.clone(),
                        RwFile::new(move |req| match req {
                            SimpleFileOperation::Read => {
                                Ok(Some(alloc::format!("{}\n", key_gc_delay()).into_bytes()))
                            }
                            SimpleFileOperation::Write(data) => {
                                if is_proc_truncate_write(data) {
                                    return Ok(None);
                                }
                                let value = write_proc_usize(data)?;
                                set_key_gc_delay(value);
                                Ok(None)
                            }
                        }),
                    ),
                );
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
                                set_key_maxkeys(value);
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
                                set_key_maxbytes(value);
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
                                set_key_root_maxkeys(value);
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
                                set_key_root_maxbytes(value);
                                Ok(None)
                            }
                        }),
                    ),
                );
                SimpleDir::new_maker(fs.clone(), Arc::new(keys))
            });
            kernel.add(
                "shmall",
                SimpleFile::new_regular(fs.clone(), || Ok(alloc::format!("{}\n", shmall_limit()))),
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
                SimpleFile::new_regular(fs.clone(), || Ok(alloc::format!("{}\n", shmmni_limit()))),
            );
            kernel.add("tainted", SimpleFile::new_regular(fs.clone(), || Ok("0\n")));
            kernel.add(
                "core_pattern",
                SimpleFile::new_regular(fs.clone(), rw_static_file("core\n")),
            );
            kernel.add(
                "printk",
                SimpleFile::new_regular(fs.clone(), rw_static_file("4 4 1 7\n")),
            );
            kernel.add("random", {
                let mut random = DirMapping::new();
                random.add(
                    "entropy_avail",
                    SimpleFile::new_regular(fs.clone(), || Ok(format!("{RANDOM_ENTROPY_BITS}\n"))),
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
