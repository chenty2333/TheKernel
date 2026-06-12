use alloc::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{any::Any, task::Context};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode, FileNodeOps, Filesystem,
    FilesystemOps, Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission, NodeType,
    Reference, StatFs, VfsError, VfsResult, WeakDirEntry, path::MAX_NAME_LEN,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use memory_addr::PAGE_SIZE_4K;
use slab::Slab;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};

use super::dummy_stat_fs;
use crate::{
    file::{Directory, OpenCredentials, current_file_write_credentials, get_typed_file},
    task::{AsThread, get_process_data, send_signal_to_process},
};

const CGROUP_SUPER_MAGIC: u32 = 0x27e0_eb;
const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;
const CPUSET_NODE_COUNT: usize = 2;

const CONTROL_FILES: &[&str] = &[
    "tasks",
    "cgroup.procs",
    "cgroup.controllers",
    "cgroup.subtree_control",
    "cgroup.clone_children",
    "cgroup.event_control",
    "cgroup.kill",
    "cgroup.sane_behavior",
    "notify_on_release",
    "release_agent",
    "cpus",
    "mems",
    "cpuset.cpus",
    "cpuset.mems",
    "cpuset.memory_migrate",
    "cpuset.cpu_exclusive",
    "cpuset.mem_exclusive",
    "cpuset.mem_hardwall",
    "cpuset.memory_pressure",
    "cpuset.memory_pressure_enabled",
    "cpuset.memory_spread_page",
    "cpuset.memory_spread_slab",
    "cpuset.sched_load_balance",
    "cpuset.sched_relax_domain_level",
    "memory.current",
    "memory.events",
    "memory.events.local",
    "memory.low",
    "memory.min",
    "memory.max",
    "memory.stat",
    "memory.swappiness",
    "memory.swap.current",
    "memory.swap.max",
    "memory.usage_in_bytes",
    "memory.limit_in_bytes",
    "memory.memsw.usage_in_bytes",
    "memory.memsw.limit_in_bytes",
    "memory.kmem.usage_in_bytes",
    "memory.kmem.limit_in_bytes",
    "memory.use_hierarchy",
    "memory.max_usage_in_bytes",
    "memory.memsw.max_usage_in_bytes",
    "memory.kmem.max_usage_in_bytes",
    "memory.failcnt",
    "memory.memsw.failcnt",
    "memory.force_empty",
    "memory.oom_control",
    "memory.soft_limit_in_bytes",
    "memory.move_charge_at_immigrate",
    "cpu.max",
    "cpu.shares",
    "cpu.cfs_quota_us",
    "cpu.cfs_period_us",
    "cpu.rt_runtime_us",
    "cpu.rt_period_us",
    "cpu.stat",
    "cpuacct.usage",
    "cpuacct.usage_percpu",
    "cpuacct.stat",
    "freezer.state",
    "freezer.self_freezing",
    "freezer.parent_freezing",
    "io.stat",
    "io.max",
    "blkio.weight",
    "blkio.weight_device",
    "blkio.throttle.read_bps_device",
    "blkio.throttle.write_bps_device",
    "blkio.throttle.read_iops_device",
    "blkio.throttle.write_iops_device",
    "blkio.throttle.io_serviced",
    "blkio.throttle.io_service_bytes",
    "blkio.io_serviced",
    "blkio.io_service_bytes",
    "blkio.io_service_time",
    "blkio.io_wait_time",
    "blkio.io_merged",
    "blkio.io_queued",
    "blkio.sectors",
    "blkio.time",
    "blkio.reset_stats",
    "devices.allow",
    "devices.deny",
    "devices.list",
    "net_cls.classid",
    "net_prio.ifpriomap",
    "net_prio.prioidx",
    "hugetlb.2MB.limit_in_bytes",
    "hugetlb.2MB.max_usage_in_bytes",
    "hugetlb.2MB.usage_in_bytes",
    "hugetlb.2MB.failcnt",
    "hugetlb.2MB.numa_stat",
    "hugetlb.2MB.rsvd.limit_in_bytes",
    "hugetlb.2MB.rsvd.max_usage_in_bytes",
    "hugetlb.2MB.rsvd.usage_in_bytes",
    "hugetlb.2MB.rsvd.failcnt",
    "hugetlb.1GB.limit_in_bytes",
    "hugetlb.1GB.max_usage_in_bytes",
    "hugetlb.1GB.usage_in_bytes",
    "hugetlb.1GB.failcnt",
    "hugetlb.1GB.numa_stat",
    "hugetlb.1GB.rsvd.limit_in_bytes",
    "hugetlb.1GB.rsvd.max_usage_in_bytes",
    "hugetlb.1GB.rsvd.usage_in_bytes",
    "hugetlb.1GB.rsvd.failcnt",
    "pids.max",
    "pids.current",
    "pids.events",
    "pids.peak",
];

const ALL_CONTROLLERS: &[&str] = &[
    "memory",
    "cpu",
    "cpuset",
    "io",
    "pids",
    "hugetlb",
    "cpuacct",
    "devices",
    "freezer",
    "net_cls",
    "net_prio",
    "blkio",
    "misc",
    "perf_event",
    "debug",
    "rdma",
];

static PID_CGROUPS: Mutex<BTreeMap<Pid, Weak<CgroupDir>>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Copy, PartialEq, Eq)]
enum CgroupVersion {
    V1,
    V2,
}

pub fn new_cgroup_v1(controllers: Vec<String>) -> Filesystem {
    CgroupFs::new(CgroupVersion::V1, controllers)
}

pub fn new_cgroup_v2() -> Filesystem {
    CgroupFs::new(
        CgroupVersion::V2,
        ALL_CONTROLLERS
            .iter()
            .map(|controller| (*controller).to_string())
            .collect(),
    )
}

pub fn parse_v1_controllers(source: &str, data: &str) -> Vec<String> {
    let mut controllers = Vec::new();
    for token in source.split(',').chain(data.split(',')) {
        let token = token.trim();
        if token.is_empty()
            || matches!(
                token,
                "none" | "cgroup" | "rw" | "ro" | "relatime" | "nosuid" | "nodev" | "noexec"
            )
        {
            continue;
        }
        if ALL_CONTROLLERS.contains(&token) && !controllers.iter().any(|it| it == token) {
            controllers.push(token.to_string());
        }
    }
    if controllers.is_empty() {
        controllers.push("memory".to_string());
    }
    controllers
}

pub fn proc_cgroups_snapshot() -> String {
    let mut out = String::from("#subsys_name\thierarchy\tnum_cgroups\tenabled\n");
    for (index, controller) in ALL_CONTROLLERS.iter().enumerate() {
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!("{controller}\t{}\t1\t1\n", index + 1),
        );
    }
    out
}

struct CgroupFs {
    name: &'static str,
    fs_type: u32,
    version: CgroupVersion,
    controllers: Vec<String>,
    inodes: Mutex<Slab<()>>,
    root: Mutex<Option<DirEntry>>,
    root_dir: Mutex<Option<Arc<CgroupDir>>>,
}

impl CgroupFs {
    fn new(version: CgroupVersion, controllers: Vec<String>) -> Filesystem {
        let fs = Arc::new(Self {
            name: match version {
                CgroupVersion::V1 => "cgroup",
                CgroupVersion::V2 => "cgroup2",
            },
            fs_type: match version {
                CgroupVersion::V1 => CGROUP_SUPER_MAGIC,
                CgroupVersion::V2 => CGROUP2_SUPER_MAGIC,
            },
            version,
            controllers,
            inodes: Mutex::new(Slab::new()),
            root: Mutex::new(None),
            root_dir: Mutex::new(None),
        });
        let root_dir = CgroupDir::new_root(fs.clone());
        *fs.root_dir.lock() = Some(root_dir.clone());
        *fs.root.lock() = Some(DirEntry::new_dir(
            |this| DirNode::new(root_dir.bind(this)),
            Reference::root(),
        ));
        Filesystem::new(fs)
    }

    fn alloc_inode(&self) -> u64 {
        self.inodes.lock().insert(()) as u64 + 1
    }

    fn release_inode(&self, ino: u64) {
        self.inodes.lock().remove(ino as usize - 1);
    }

    fn remove_pid_everywhere(&self, pid: Pid) {
        if let Some(root) = self.root_dir.lock().clone() {
            root.remove_pid_recursive(pid);
        }
    }
}

impl FilesystemOps for CgroupFs {
    fn name(&self) -> &str {
        self.name
    }

    fn root_dir(&self) -> DirEntry {
        self.root.lock().clone().unwrap()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(dummy_stat_fs(self.fs_type))
    }
}

struct CgroupNode {
    fs: Arc<CgroupFs>,
    ino: u64,
    metadata: Mutex<Metadata>,
}

impl CgroupNode {
    fn new(fs: Arc<CgroupFs>, node_type: NodeType, mode: NodePermission) -> Self {
        let ino = fs.alloc_inode();
        let now = wall_time();
        let metadata = Metadata {
            device: 0,
            inode: ino,
            nlink: 1,
            mode,
            node_type,
            uid: 0,
            gid: 0,
            size: 0,
            block_size: 0,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: now,
            btime: now,
            mtime: now,
            ctime: now,
        };
        Self {
            fs,
            ino,
            metadata: Mutex::new(metadata),
        }
    }

    fn metadata(&self) -> Metadata {
        self.metadata.lock().clone()
    }

    fn update_metadata(&self, update: MetadataUpdate) {
        let mut metadata = self.metadata.lock();
        let mut status_changed = false;
        if let Some(mode) = update.mode {
            metadata.mode = mode;
            status_changed = true;
        }
        if let Some((uid, gid)) = update.owner {
            metadata.uid = uid;
            metadata.gid = gid;
            status_changed = true;
        }
        if let Some(rdev) = update.rdev {
            metadata.rdev = rdev;
            status_changed = true;
        }
        if let Some(atime) = update.atime {
            metadata.atime = atime;
        }
        if let Some(mtime) = update.mtime {
            metadata.mtime = mtime;
            status_changed = true;
        }
        if let Some(ctime) = update.ctime {
            metadata.ctime = ctime;
        } else if status_changed {
            metadata.ctime = wall_time();
        }
    }
}

impl Drop for CgroupNode {
    fn drop(&mut self) {
        self.fs.release_inode(self.ino);
    }
}

struct CgroupDir {
    node: CgroupNode,
    parent: Option<Weak<CgroupDir>>,
    this: Mutex<Option<WeakDirEntry>>,
    children: Mutex<BTreeMap<String, Arc<CgroupDir>>>,
    files: BTreeMap<String, Arc<CgroupFile>>,
    pids: Mutex<BTreeSet<Pid>>,
    pids_max: Mutex<Option<u64>>,
    pids_peak: Mutex<u64>,
    pids_events_limit: Mutex<u64>,
    subtree_control: Mutex<BTreeSet<String>>,
    memory_max_usage: Mutex<u64>,
    memory_memsw_max_usage: Mutex<u64>,
    memory_failcnt: Mutex<u64>,
    memory_memsw_failcnt: Mutex<u64>,
    cpuacct_usage_ns: Mutex<u64>,
}

impl CgroupDir {
    fn new_root(fs: Arc<CgroupFs>) -> Arc<Self> {
        let dir = Self::new(fs, None);
        dir.init_root_cpuset_defaults();
        dir
    }

    fn new(fs: Arc<CgroupFs>, parent: Option<Weak<CgroupDir>>) -> Arc<Self> {
        let parent_dir = parent.as_ref().and_then(Weak::upgrade);
        let mode = NodePermission::from_bits_truncate(0o755);
        let files = CONTROL_FILES
            .iter()
            .map(|name| ((*name).to_string(), CgroupFile::new(fs.clone(), *name)))
            .collect();
        let dir = Arc::new(Self {
            node: CgroupNode::new(fs, NodeType::Directory, mode),
            parent,
            this: Mutex::new(None),
            children: Mutex::new(BTreeMap::new()),
            files,
            pids: Mutex::new(BTreeSet::new()),
            pids_max: Mutex::new(None),
            pids_peak: Mutex::new(0),
            pids_events_limit: Mutex::new(0),
            subtree_control: Mutex::new(BTreeSet::new()),
            memory_max_usage: Mutex::new(0),
            memory_memsw_max_usage: Mutex::new(0),
            memory_failcnt: Mutex::new(0),
            memory_memsw_failcnt: Mutex::new(0),
            cpuacct_usage_ns: Mutex::new(0),
        });
        dir.bind_control_files();
        if let Some(parent) = parent_dir {
            dir.inherit_from_parent(&parent);
        }
        dir
    }

    fn bind(self: &Arc<Self>, this: WeakDirEntry) -> Arc<Self> {
        *self.this.lock() = Some(this);
        self.clone()
    }

    fn reference(&self, name: &str) -> Reference {
        Reference::new(
            self.this.lock().as_ref().and_then(WeakDirEntry::upgrade),
            name.to_string(),
        )
    }

    fn live_pids(&self) -> Vec<Pid> {
        let mut pids = self.pids.lock();
        pids.retain(|pid| get_process_data(*pid).is_ok());
        pids.iter().copied().collect()
    }

    fn remove_pid_recursive(&self, pid: Pid) {
        self.pids.lock().remove(&pid);
        for child in self.children.lock().values() {
            child.remove_pid_recursive(pid);
        }
    }

    fn recursive_live_pid_count(&self) -> usize {
        let local = self.live_pids().len();
        let children = self.children.lock().values().cloned().collect::<Vec<_>>();
        local
            + children
                .iter()
                .map(|child| child.recursive_live_pid_count())
                .sum::<usize>()
    }

    fn local_memory_current_bytes(&self) -> u64 {
        self.live_pids()
            .into_iter()
            .filter_map(|pid| get_process_data(pid).ok())
            .map(|proc_data| proc_data.aspace().lock().resident_user_bytes() as u64)
            .sum()
    }

    fn recursive_memory_current_bytes(&self) -> u64 {
        let local = self.local_memory_current_bytes();
        let children = self.children.lock().values().cloned().collect::<Vec<_>>();
        local
            + children
                .iter()
                .map(|child| child.recursive_memory_current_bytes())
                .sum::<u64>()
    }

    fn note_memory_usage(&self, current: u64) {
        {
            let mut max = self.memory_max_usage.lock();
            *max = (*max).max(current);
        }
        {
            let mut max = self.memory_memsw_max_usage.lock();
            *max = (*max).max(current);
        }
        if self
            .memory_limit_bytes("memory.limit_in_bytes")
            .is_some_and(|limit| current > limit)
        {
            *self.memory_failcnt.lock() += 1;
        }
        if self
            .memory_limit_bytes("memory.memsw.limit_in_bytes")
            .is_some_and(|limit| current > limit)
        {
            *self.memory_memsw_failcnt.lock() += 1;
        }
    }

    fn memory_current_bytes(&self) -> u64 {
        let current = self.recursive_memory_current_bytes();
        self.note_memory_usage(current);
        current
    }

    fn memory_max_usage_bytes(&self, memsw: bool) -> u64 {
        let current = self.memory_current_bytes();
        if memsw {
            let mut max = self.memory_memsw_max_usage.lock();
            *max = (*max).max(current);
            *max
        } else {
            let mut max = self.memory_max_usage.lock();
            *max = (*max).max(current);
            *max
        }
    }

    fn reset_memory_max_usage(&self, memsw: bool) {
        let current = self.memory_current_bytes();
        if memsw {
            *self.memory_memsw_max_usage.lock() = current;
        } else {
            *self.memory_max_usage.lock() = current;
        }
    }

    fn memory_limit_bytes(&self, name: &str) -> Option<u64> {
        let value = self.stored_control_text(name)?;
        let value = value.trim();
        if value == "max" {
            None
        } else {
            value.parse::<u64>().ok()
        }
    }

    fn memory_stat_text(&self) -> String {
        let anon = self.memory_current_bytes();
        format!(
            "anon {anon}\nfile 0\nkernel 0\nkernel_stack 0\npagetables 0\npercpu 0\nsock \
             0\nvmalloc 0\nshmem 0\nfile_mapped 0\nfile_dirty 0\nfile_writeback 0\nswapcached \
             0\nanon_thp 0\nfile_thp 0\nshmem_thp 0\ninactive_anon {anon}\nactive_anon \
             0\ninactive_file 0\nactive_file 0\nunevictable 0\nslab_reclaimable \
             0\nslab_unreclaimable 0\nslab 0\nworkingset_refault_anon 0\nworkingset_refault_file \
             0\nworkingset_activate_anon 0\nworkingset_activate_file 0\nworkingset_restore_anon \
             0\nworkingset_restore_file 0\nworkingset_nodereclaim 0\npgfault 0\npgmajfault 0\nrss \
             {anon}\ncache 0\nmapped_file 0\n"
        )
    }

    fn memory_events_text(&self) -> String {
        "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n".to_string()
    }

    fn note_cpuacct_charge(&self) {
        let mut usage = self.cpuacct_usage_ns.lock();
        *usage = usage.saturating_add(1_000_000);
    }

    fn recursive_cpuacct_usage_ns(&self) -> u64 {
        let local = *self.cpuacct_usage_ns.lock();
        let children = self.children.lock().values().cloned().collect::<Vec<_>>();
        local
            + children
                .iter()
                .map(|child| child.recursive_cpuacct_usage_ns())
                .sum::<u64>()
    }

    fn cpuacct_usage_percpu_text(&self) -> String {
        let cpus = axhal::cpu_num().max(1);
        let total = self.recursive_cpuacct_usage_ns();
        let base = total / cpus as u64;
        let extra = total % cpus as u64;
        let mut values = Vec::new();
        for cpu in 0..cpus {
            let value = base + u64::from((cpu as u64) < extra);
            values.push(value.to_string());
        }
        format!("{}\n", values.join(" "))
    }

    fn cpuacct_stat_text(&self) -> String {
        let ticks = self.recursive_cpuacct_usage_ns() / 10_000_000;
        format!("user {ticks}\nsystem 0\n")
    }

    fn update_pids_peak(&self, count: usize) {
        let mut peak = self.pids_peak.lock();
        *peak = (*peak).max(count as u64);
    }

    fn update_pids_peak_hierarchy(self: &Arc<Self>) {
        let mut current = Some(self.clone());
        while let Some(dir) = current {
            dir.update_pids_peak(dir.recursive_live_pid_count());
            current = dir.parent.as_ref().and_then(Weak::upgrade);
        }
    }

    fn limiting_dir_for_fork(self: &Arc<Self>) -> Option<Arc<CgroupDir>> {
        let mut current = Some(self.clone());
        while let Some(dir) = current {
            let limit = *dir.pids_max.lock();
            if let Some(limit) = limit
                && dir.recursive_live_pid_count() as u64 + 1 > limit
            {
                return Some(dir);
            }
            current = dir.parent.as_ref().and_then(Weak::upgrade);
        }
        None
    }

    fn has_real_children(&self) -> bool {
        !self.children.lock().is_empty()
    }

    fn v2_has_enabled_child_controllers(&self) -> bool {
        self.node.fs.version == CgroupVersion::V2
            && self.parent.is_some()
            && !self.subtree_control.lock().is_empty()
    }

    fn attach_pid(&self, pid: Pid) -> VfsResult<()> {
        let pid = if pid == 0 {
            axtask::current().as_thread().proc_data.proc.pid()
        } else {
            pid
        };
        if self.v2_has_enabled_child_controllers() {
            return Err(VfsError::ResourceBusy);
        }
        let target = get_process_data(pid).map_err(|_| VfsError::NotFound)?;
        let credentials = current_file_write_credentials().unwrap_or_else(OpenCredentials::current);
        if !can_migrate_from_open_cgroup_namespace(credentials) {
            return Err(VfsError::NotFound);
        }
        if !can_migrate_with_credentials(credentials, &target) {
            return Err(VfsError::PermissionDenied);
        }
        if self.requires_cpuset_placement()
            && (self.cpuset_mask("cpuset.cpus").is_empty()
                || self.cpuset_mask("cpuset.mems").is_empty())
        {
            return Err(VfsError::InvalidInput);
        }
        let this = self.this_dir()?;
        detach_mapped_pid(pid);
        self.node.fs.remove_pid_everywhere(pid);
        self.pids.lock().insert(pid);
        self.note_cpuacct_charge();
        PID_CGROUPS.lock().insert(pid, Arc::downgrade(&this));
        this.update_pids_peak_hierarchy();
        Ok(())
    }

    fn attach_fork_child(self: &Arc<Self>, pid: Pid) {
        self.pids.lock().insert(pid);
        self.note_cpuacct_charge();
        PID_CGROUPS.lock().insert(pid, Arc::downgrade(self));
        self.update_pids_peak_hierarchy();
    }

    fn kill_attached(&self) {
        for pid in self.live_pids() {
            let _ = send_signal_to_process(pid, Some(SignalInfo::new_kernel(Signo::SIGKILL)));
        }
    }

    fn kill_attached_recursive(&self) {
        self.kill_attached();
        let children = self.children.lock().values().cloned().collect::<Vec<_>>();
        for child in children {
            child.kill_attached_recursive();
        }
    }

    fn tasks_text(&self) -> String {
        let mut out = String::new();
        for pid in self.live_pids() {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{pid}\n"));
        }
        out
    }

    fn subtree_control_text(&self) -> String {
        let mut out = self
            .subtree_control
            .lock()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    fn available_controllers(&self) -> BTreeSet<String> {
        if self.node.fs.version == CgroupVersion::V1 {
            return BTreeSet::new();
        }
        if let Some(parent) = self.parent.as_ref().and_then(Weak::upgrade) {
            return parent.subtree_control.lock().clone();
        }
        self.node.fs.controllers.iter().cloned().collect()
    }

    fn controllers_text(&self) -> String {
        let mut out = self
            .available_controllers()
            .into_iter()
            .collect::<Vec<_>>()
            .join(" ");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    fn pids_max_text(&self) -> String {
        match *self.pids_max.lock() {
            Some(limit) => format!("{limit}\n"),
            None => "max\n".to_string(),
        }
    }

    fn set_pids_max(&self, data: &[u8]) -> VfsResult<()> {
        let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
        let text = text.trim();
        let value = if text == "max" {
            None
        } else {
            if text.starts_with('-') {
                return Err(VfsError::InvalidInput);
            }
            Some(text.parse::<u64>().map_err(|_| VfsError::InvalidInput)?)
        };
        *self.pids_max.lock() = value;
        Ok(())
    }

    fn child_has_subtree_controller(&self, name: &str) -> bool {
        self.children
            .lock()
            .values()
            .any(|child| child.subtree_control.lock().contains(name))
    }

    fn update_subtree_control(&self, data: &[u8]) -> VfsResult<()> {
        let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
        let available = self.available_controllers();
        let mut enable = BTreeSet::new();
        let mut disable = BTreeSet::new();
        for token in text.split_ascii_whitespace() {
            if token.len() < 2 {
                return Err(VfsError::InvalidInput);
            }
            let (op, name) = token.split_at(1);
            match op {
                "+" => {
                    if !available.contains(name) {
                        return Err(VfsError::NotFound);
                    }
                    enable.insert(name.to_string());
                    disable.remove(name);
                }
                "-" => {
                    disable.insert(name.to_string());
                    enable.remove(name);
                }
                _ => return Err(VfsError::InvalidInput),
            }
        }

        let mut next = self.subtree_control.lock().clone();
        if self.node.fs.version == CgroupVersion::V2
            && self.parent.is_some()
            && enable.iter().any(|name| !next.contains(name))
            && !self.live_pids().is_empty()
        {
            return Err(VfsError::ResourceBusy);
        }
        for name in &disable {
            if next.contains(name) && self.child_has_subtree_controller(name) {
                return Err(VfsError::ResourceBusy);
            }
        }
        for name in enable {
            next.insert(name);
        }
        for name in disable {
            next.remove(&name);
        }
        *self.subtree_control.lock() = next;
        Ok(())
    }

    fn stored_control_text(&self, name: &str) -> Option<String> {
        self.files.get(name).map(|file| file.value.lock().clone())
    }

    fn memory_hierarchy_enabled(&self) -> bool {
        self.stored_control_text("memory.use_hierarchy")
            .is_some_and(|value| value.trim() == "1")
    }

    fn ancestor_memory_hierarchy_enabled(&self) -> bool {
        let mut current = self.parent.as_ref().and_then(Weak::upgrade);
        while let Some(dir) = current {
            if dir.memory_hierarchy_enabled() {
                return true;
            }
            current = dir.parent.as_ref().and_then(Weak::upgrade);
        }
        false
    }

    fn inherit_from_parent(&self, parent: &CgroupDir) {
        self.copy_control_value(parent, "notify_on_release");
        self.copy_control_value(parent, "cpuset.memory_spread_page");
        self.copy_control_value(parent, "cpuset.memory_spread_slab");
        if parent
            .stored_control_text("cgroup.clone_children")
            .is_some_and(|value| value.trim() == "1")
        {
            self.copy_control_value(parent, "cgroup.clone_children");
            for name in [
                "cpus",
                "mems",
                "cpuset.cpus",
                "cpuset.mems",
                "cpuset.memory_migrate",
            ] {
                self.copy_control_value(parent, name);
            }
        }
    }

    fn copy_control_value(&self, parent: &CgroupDir, name: &str) {
        let Some(value) = parent.stored_control_text(name) else {
            return;
        };
        self.set_control_value(name, value);
    }

    fn init_root_cpuset_defaults(&self) {
        let cpus = cpuset_topology_mask(axhal::cpu_num().max(1));
        let mems = cpuset_topology_mask(CPUSET_NODE_COUNT);
        for name in ["cpus", "cpuset.cpus"] {
            self.set_control_value(name, cpus.clone());
        }
        for name in ["mems", "cpuset.mems"] {
            self.set_control_value(name, mems.clone());
        }
    }

    fn set_control_value(&self, name: &str, value: String) {
        if let Some(file) = self.files.get(name) {
            *file.value.lock() = value;
        }
    }

    fn control_bool(&self, name: &str) -> bool {
        self.stored_control_text(name)
            .is_some_and(|value| value.trim() != "0")
    }

    fn requires_cpuset_placement(&self) -> bool {
        self.node.fs.version == CgroupVersion::V1
            && self
                .node
                .fs
                .controllers
                .iter()
                .any(|controller| controller == "cpuset")
    }

    fn cpuset_mask(&self, name: &str) -> BTreeSet<u32> {
        let max = cpuset_mask_max(name);
        self.stored_control_text(name)
            .and_then(|value| parse_cpuset_mask(value.as_bytes(), max).ok())
            .map(|(mask, _)| mask)
            .unwrap_or_default()
    }

    fn validate_exclusive_flag_update(
        &self,
        flag_name: &str,
        mask_name: &str,
        value: bool,
    ) -> VfsResult<()> {
        if value {
            if let Some(parent) = self.parent.as_ref().and_then(Weak::upgrade) {
                if !parent.control_bool(flag_name) {
                    return Err(VfsError::InvalidInput);
                }
                let mask = self.cpuset_mask(mask_name);
                for sibling in parent.children.lock().values() {
                    if sibling.node.ino == self.node.ino {
                        continue;
                    }
                    if masks_overlap(&mask, &sibling.cpuset_mask(mask_name)) {
                        return Err(VfsError::InvalidInput);
                    }
                }
            }
        } else {
            for child in self.children.lock().values() {
                if child.control_bool(flag_name) {
                    return Err(VfsError::InvalidInput);
                }
            }
        }
        Ok(())
    }

    fn validate_cpuset_mask_update(
        &self,
        mask_name: &str,
        candidate: &BTreeSet<u32>,
    ) -> VfsResult<()> {
        let flag_name = cpuset_exclusive_flag_for_mask(mask_name);
        let Some(parent) = self.parent.as_ref().and_then(Weak::upgrade) else {
            return Ok(());
        };
        if !parent.control_bool(flag_name) {
            return Ok(());
        }
        let self_exclusive = self.control_bool(flag_name);
        for sibling in parent.children.lock().values() {
            if sibling.node.ino == self.node.ino {
                continue;
            }
            if (self_exclusive || sibling.control_bool(flag_name))
                && masks_overlap(candidate, &sibling.cpuset_mask(mask_name))
            {
                return Err(VfsError::InvalidInput);
            }
        }
        Ok(())
    }
}

fn can_migrate_with_credentials(
    credentials: OpenCredentials,
    target: &crate::task::ProcessData,
) -> bool {
    credentials.euid == 0
        || credentials.fsuid == 0
        || [
            credentials.uid,
            credentials.euid,
            credentials.suid,
            credentials.fsuid,
        ]
        .into_iter()
        .any(|uid| uid == target.uid() || uid == target.euid() || uid == target.suid())
}

fn can_migrate_from_open_cgroup_namespace(credentials: OpenCredentials) -> bool {
    credentials.cgroup_ns_id == 0
        || credentials.cgroup_ns_id == axtask::current().as_thread().proc_data.cgroup_ns_id()
}

fn detach_mapped_pid(pid: Pid) {
    let old = PID_CGROUPS
        .lock()
        .remove(&pid)
        .and_then(|weak| weak.upgrade());
    if let Some(dir) = old {
        dir.pids.lock().remove(&pid);
    }
}

fn cgroup_for_pid(pid: Pid) -> Option<Arc<CgroupDir>> {
    let weak = PID_CGROUPS.lock().get(&pid).cloned()?;
    let Some(dir) = weak.upgrade() else {
        PID_CGROUPS.lock().remove(&pid);
        return None;
    };
    if get_process_data(pid).is_err() {
        PID_CGROUPS.lock().remove(&pid);
        dir.pids.lock().remove(&pid);
        return None;
    }
    Some(dir)
}

fn cgroup_relative_path(dir: &CgroupDir) -> String {
    dir.this
        .lock()
        .as_ref()
        .and_then(WeakDirEntry::upgrade)
        .and_then(|entry| entry.absolute_path().ok())
        .map(|path| path.to_string())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "/".to_string())
}

fn cgroup_hierarchy_id(controllers: &[String]) -> usize {
    controllers
        .first()
        .and_then(|controller| ALL_CONTROLLERS.iter().position(|it| *it == controller))
        .map_or(1, |index| index + 1)
}

pub(crate) fn proc_cgroup_membership(pid: Pid) -> String {
    let Some(dir) = cgroup_for_pid(pid) else {
        return "0::/\n".to_string();
    };
    let path = cgroup_relative_path(&dir);
    match dir.node.fs.version {
        CgroupVersion::V2 => format!("0::{path}\n"),
        CgroupVersion::V1 => {
            let controllers = dir.node.fs.controllers.join(",");
            let hierarchy = cgroup_hierarchy_id(&dir.node.fs.controllers);
            format!("{hierarchy}:{controllers}:{path}\n")
        }
    }
}

pub(crate) fn proc_cpuset_membership(pid: Pid) -> String {
    cgroup_for_pid(pid)
        .map(|dir| format!("{}\n", cgroup_relative_path(&dir)))
        .unwrap_or_else(|| "/\n".to_string())
}

fn cpuset_mask_bits(mask: &BTreeSet<u32>) -> usize {
    let mut bits = 0usize;
    for index in mask {
        bits |= 1usize.checked_shl(*index).unwrap_or(0);
    }
    bits
}

pub(crate) fn cpuset_allowed_masks(pid: Pid) -> Option<(usize, usize)> {
    let dir = cgroup_for_pid(pid)?;
    let mut cpus = dir.cpuset_mask("cpuset.cpus");
    if cpus.is_empty() {
        cpus = dir.cpuset_mask("cpus");
    }
    let mut mems = dir.cpuset_mask("cpuset.mems");
    if mems.is_empty() {
        mems = dir.cpuset_mask("mems");
    }
    if cpus.is_empty() || mems.is_empty() {
        return None;
    }
    Some((cpuset_mask_bits(&cpus), cpuset_mask_bits(&mems)))
}

pub fn try_charge_fork(parent_pid: Pid, child_pid: Pid) -> AxResult<()> {
    let Some(dir) = cgroup_for_pid(parent_pid) else {
        return Ok(());
    };
    if let Some(limiting) = dir.limiting_dir_for_fork() {
        *limiting.pids_events_limit.lock() += 1;
        return Err(AxError::WouldBlock);
    }
    dir.attach_fork_child(child_pid);
    Ok(())
}

pub fn try_charge_fork_into(cgroup_fd: i32, child_pid: Pid) -> AxResult<()> {
    let dir_file = get_typed_file::<Directory>(cgroup_fd)?;
    let dir = dir_file
        .inner()
        .entry()
        .downcast::<CgroupDir>()
        .map_err(|_| AxError::InvalidInput)?;
    if dir.node.fs.version != CgroupVersion::V2 {
        return Err(AxError::InvalidInput);
    }
    if dir.v2_has_enabled_child_controllers() {
        return Err(AxError::ResourceBusy);
    }
    if let Some(limiting) = dir.limiting_dir_for_fork() {
        *limiting.pids_events_limit.lock() += 1;
        return Err(AxError::WouldBlock);
    }
    dir.attach_fork_child(child_pid);
    Ok(())
}

impl NodeOps for CgroupDir {
    fn inode(&self) -> u64 {
        self.node.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        Ok(self.node.metadata())
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.node.update_metadata(update);
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.node.fs.as_ref()
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

impl DirNodeOps for CgroupDir {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let parent_ino = self
            .parent
            .as_ref()
            .and_then(Weak::upgrade)
            .map_or(self.node.ino, |parent| parent.node.ino);
        let mut entries: Vec<(Cow<'_, str>, u64, NodeType)> = Vec::new();
        entries.push((".".into(), self.node.ino, NodeType::Directory));
        entries.push(("..".into(), parent_ino, NodeType::Directory));
        entries.extend(
            self.children
                .lock()
                .iter()
                .map(|(name, dir)| (Cow::Owned(name.clone()), dir.node.ino, NodeType::Directory)),
        );
        entries.extend(self.files.iter().map(|(name, file)| {
            (
                Cow::Owned(name.clone()),
                file.node.ino,
                NodeType::RegularFile,
            )
        }));

        let mut count = 0;
        for (index, (name, ino, node_type)) in entries.into_iter().enumerate().skip(offset as usize)
        {
            if !sink.accept(&name, ino, node_type, index as u64 + 1) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        if let Some(child) = self.children.lock().get(name).cloned() {
            return Ok(DirEntry::new_dir(
                |this| DirNode::new(child.bind(this)),
                self.reference(name),
            ));
        }
        if let Some(file) = self.files.get(name).cloned() {
            return Ok(DirEntry::new_file(
                FileNode::new(file),
                NodeType::RegularFile,
                self.reference(name),
            ));
        }
        Err(VfsError::NotFound)
    }

    fn create(
        &self,
        name: &str,
        node_type: NodeType,
        _permission: NodePermission,
    ) -> VfsResult<DirEntry> {
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if name.contains('\n') {
            return Err(VfsError::InvalidInput);
        }
        if node_type != NodeType::Directory {
            return Err(VfsError::OperationNotPermitted);
        }
        let mut children = self.children.lock();
        if children.contains_key(name) || self.files.contains_key(name) {
            return Err(VfsError::AlreadyExists);
        }
        let child = Self::new(
            self.node.fs.clone(),
            Some(Arc::downgrade(&self.this_dir()?)),
        );
        children.insert(name.to_string(), child.clone());
        Ok(DirEntry::new_dir(
            |this| DirNode::new(child.bind(this)),
            self.reference(name),
        ))
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        if self.files.contains_key(name) {
            return Err(VfsError::OperationNotPermitted);
        }
        let Some(child) = self.children.lock().get(name).cloned() else {
            return Err(VfsError::NotFound);
        };
        if child.has_real_children() {
            return Err(VfsError::DirectoryNotEmpty);
        }
        if !child.live_pids().is_empty() {
            return Err(VfsError::ResourceBusy);
        }
        self.children.lock().remove(name);
        Ok(())
    }

    fn rename(&self, src_name: &str, dst_dir: &DirNode, dst_name: &str) -> VfsResult<()> {
        let dst_dir = dst_dir.downcast::<Self>()?;
        if !Arc::ptr_eq(&self.node.fs, &dst_dir.node.fs) {
            return Err(VfsError::CrossesDevices);
        }
        if self.node.fs.version != CgroupVersion::V1 {
            return Err(VfsError::OperationNotPermitted);
        }
        if self.node.ino != dst_dir.node.ino {
            return Err(VfsError::Io);
        }
        if dst_name.contains('\n') {
            return Err(VfsError::InvalidInput);
        }
        if self.files.contains_key(src_name) || dst_dir.files.contains_key(dst_name) {
            return Err(VfsError::OperationNotPermitted);
        }
        if dst_dir.children.lock().contains_key(dst_name) {
            return Err(VfsError::AlreadyExists);
        }
        let child = self
            .children
            .lock()
            .remove(src_name)
            .ok_or(VfsError::NotFound)?;
        dst_dir.children.lock().insert(dst_name.to_string(), child);
        Ok(())
    }

    fn is_cacheable(&self) -> bool {
        true
    }
}

impl CgroupDir {
    fn this_dir(&self) -> VfsResult<Arc<CgroupDir>> {
        self.this
            .lock()
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::InvalidInput)?
            .downcast::<CgroupDir>()
    }
}

struct CgroupFile {
    node: CgroupNode,
    name: &'static str,
    value: Mutex<String>,
    dir: Mutex<Option<Weak<CgroupDir>>>,
}

impl CgroupFile {
    fn new(fs: Arc<CgroupFs>, name: &'static str) -> Arc<Self> {
        let mode = NodePermission::from_bits_truncate(if is_read_only_control_file(name) {
            0o444
        } else {
            0o644
        });
        Arc::new(Self {
            node: CgroupNode::new(fs, NodeType::RegularFile, mode),
            name,
            value: Mutex::new(default_value(name)),
            dir: Mutex::new(None),
        })
    }

    fn bind_dir(&self, dir: &Arc<CgroupDir>) {
        let mut slot = self.dir.lock();
        if slot.is_none() {
            *slot = Some(Arc::downgrade(dir));
        }
    }

    fn dir(&self) -> VfsResult<Arc<CgroupDir>> {
        self.dir
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or(VfsError::InvalidInput)
    }

    fn read_text(&self) -> VfsResult<String> {
        let dir = self.dir()?;
        Ok(match self.name {
            "tasks" | "cgroup.procs" => dir.tasks_text(),
            "cgroup.controllers" => dir.controllers_text(),
            "cgroup.subtree_control" => dir.subtree_control_text(),
            "memory.current" | "memory.usage_in_bytes" | "memory.memsw.usage_in_bytes" => {
                format!("{}\n", dir.memory_current_bytes())
            }
            "memory.kmem.usage_in_bytes" => "0\n".to_string(),
            "memory.max_usage_in_bytes" => format!("{}\n", dir.memory_max_usage_bytes(false)),
            "memory.memsw.max_usage_in_bytes" => {
                format!("{}\n", dir.memory_max_usage_bytes(true))
            }
            "memory.kmem.max_usage_in_bytes" => "0\n".to_string(),
            "memory.failcnt" => format!("{}\n", *dir.memory_failcnt.lock()),
            "memory.memsw.failcnt" => format!("{}\n", *dir.memory_memsw_failcnt.lock()),
            "memory.events" | "memory.events.local" => dir.memory_events_text(),
            "memory.stat" => dir.memory_stat_text(),
            "cpuacct.usage" => format!("{}\n", dir.recursive_cpuacct_usage_ns()),
            "cpuacct.usage_percpu" => dir.cpuacct_usage_percpu_text(),
            "cpuacct.stat" => dir.cpuacct_stat_text(),
            "freezer.self_freezing" => {
                let state = self.value.lock();
                freezer_self_freezing_text(&state)
            }
            "freezer.parent_freezing" => "0\n".to_string(),
            "blkio.throttle.io_serviced"
            | "blkio.throttle.io_service_bytes"
            | "blkio.io_serviced"
            | "blkio.io_service_bytes"
            | "blkio.io_service_time"
            | "blkio.io_wait_time"
            | "blkio.io_merged"
            | "blkio.io_queued"
            | "blkio.sectors"
            | "blkio.time" => "Total 0\n".to_string(),
            name if name.starts_with("hugetlb.") && name.ends_with(".numa_stat") => {
                "total=0 N0=0\n".to_string()
            }
            "pids.max" => dir.pids_max_text(),
            "pids.current" => format!("{}\n", dir.recursive_live_pid_count()),
            "pids.events" => format!("max {}\n", *dir.pids_events_limit.lock()),
            "pids.peak" => format!("{}\n", *dir.pids_peak.lock()),
            _ => self.value.lock().clone(),
        })
    }

    fn write_text(&self, data: &[u8]) -> VfsResult<()> {
        let dir = self.dir()?;
        match self.name {
            "tasks" | "cgroup.procs" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                let pid = text
                    .trim()
                    .parse::<Pid>()
                    .map_err(|_| VfsError::InvalidInput)?;
                dir.attach_pid(pid)
            }
            "cgroup.kill" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                if text.trim() != "1" {
                    return Err(VfsError::InvalidInput);
                }
                dir.kill_attached_recursive();
                Ok(())
            }
            "cgroup.event_control" => Ok(()),
            "cgroup.subtree_control" => dir.update_subtree_control(data),
            "pids.max" => dir.set_pids_max(data),
            "cpu.shares" => {
                if dir.parent.is_none() {
                    return Err(VfsError::InvalidInput);
                }
                let shares = parse_cgroup_u64(data)?;
                let shares = shares.clamp(2, 1 << 18);
                *self.value.lock() = format!("{shares}\n");
                Ok(())
            }
            "cpu.cfs_quota_us" => {
                let value = parse_signed_cgroup_i64(data)?;
                if value < -1 {
                    return Err(VfsError::InvalidInput);
                }
                *self.value.lock() = format!("{value}\n");
                Ok(())
            }
            "cpu.cfs_period_us" | "cpu.rt_period_us" => {
                let value = parse_cgroup_u64(data)?;
                if !(1_000..=1_000_000).contains(&value) {
                    return Err(VfsError::InvalidInput);
                }
                *self.value.lock() = format!("{value}\n");
                Ok(())
            }
            "cpu.rt_runtime_us" => {
                let value = parse_signed_cgroup_i64(data)?;
                if value < -1 {
                    return Err(VfsError::InvalidInput);
                }
                *self.value.lock() = format!("{value}\n");
                Ok(())
            }
            "freezer.state" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                match text.trim() {
                    "THAWED" | "FROZEN" => {
                        *self.value.lock() = format!("{}\n", text.trim());
                        Ok(())
                    }
                    _ => Err(VfsError::InvalidInput),
                }
            }
            "net_cls.classid" => {
                let value = parse_classid(data)?;
                *self.value.lock() = format!("{value}\n");
                Ok(())
            }
            name if name.starts_with("hugetlb.")
                && (name.ends_with(".limit_in_bytes")
                    || name.ends_with(".rsvd.limit_in_bytes")) =>
            {
                *self.value.lock() = parse_memory_control_value(data)?;
                Ok(())
            }
            "cpus" | "cpuset.cpus" | "mems" | "cpuset.mems" => {
                if dir.parent.is_none() {
                    return Err(VfsError::BadFileDescriptor);
                }
                let (mask, value) = parse_cpuset_mask(data, cpuset_mask_max(self.name))?;
                dir.validate_cpuset_mask_update(self.name, &mask)?;
                *self.value.lock() = value;
                Ok(())
            }
            "memory.use_hierarchy" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                let value = text.trim();
                match value {
                    "1" if dir.has_real_children() => Err(VfsError::InvalidInput),
                    "0" if dir.ancestor_memory_hierarchy_enabled() => Err(VfsError::InvalidInput),
                    "0" | "1" => {
                        *self.value.lock() = format!("{value}\n");
                        Ok(())
                    }
                    _ => Err(VfsError::InvalidInput),
                }
            }
            "notify_on_release" | "cgroup.clone_children" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                match text.trim() {
                    "0" | "1" => {
                        *self.value.lock() = format!("{}\n", text.trim());
                        Ok(())
                    }
                    _ => Err(VfsError::InvalidInput),
                }
            }
            "cpuset.cpu_exclusive" | "cpuset.mem_exclusive" => {
                let value = parse_cpuset_flag(data)?;
                let mask_name = if self.name == "cpuset.cpu_exclusive" {
                    "cpuset.cpus"
                } else {
                    "cpuset.mems"
                };
                dir.validate_exclusive_flag_update(self.name, mask_name, value)?;
                *self.value.lock() = flag_text(value);
                Ok(())
            }
            "cpuset.mem_hardwall"
            | "cpuset.memory_migrate"
            | "cpuset.memory_pressure_enabled"
            | "cpuset.memory_spread_page"
            | "cpuset.memory_spread_slab"
            | "cpuset.sched_load_balance" => {
                *self.value.lock() = flag_text(parse_cpuset_flag(data)?);
                Ok(())
            }
            "cpuset.sched_relax_domain_level" => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                let value = text
                    .trim()
                    .parse::<i32>()
                    .map_err(|_| VfsError::InvalidInput)?;
                if !(-1..=5).contains(&value) {
                    return Err(VfsError::InvalidInput);
                }
                *self.value.lock() = format!("{value}\n");
                Ok(())
            }
            "memory.force_empty" => {
                if dir.parent.is_none() {
                    Err(VfsError::InvalidInput)
                } else {
                    Ok(())
                }
            }
            "memory.max_usage_in_bytes" => {
                dir.reset_memory_max_usage(false);
                Ok(())
            }
            "memory.memsw.max_usage_in_bytes" => {
                dir.reset_memory_max_usage(true);
                Ok(())
            }
            "memory.failcnt" => {
                *dir.memory_failcnt.lock() = 0;
                Ok(())
            }
            "memory.memsw.failcnt" => {
                *dir.memory_memsw_failcnt.lock() = 0;
                Ok(())
            }
            "memory.low"
            | "memory.min"
            | "memory.max"
            | "memory.swap.max"
            | "memory.limit_in_bytes"
            | "memory.memsw.limit_in_bytes"
            | "memory.kmem.limit_in_bytes"
            | "memory.soft_limit_in_bytes" => {
                *self.value.lock() = parse_memory_control_value(data)?;
                Ok(())
            }
            "cgroup.controllers"
            | "cgroup.sane_behavior"
            | "cpuset.memory_pressure"
            | "cpu.stat"
            | "cpuacct.usage"
            | "cpuacct.usage_percpu"
            | "cpuacct.stat"
            | "freezer.self_freezing"
            | "freezer.parent_freezing"
            | "devices.list"
            | "net_prio.prioidx"
            | "pids.current"
            | "pids.events"
            | "pids.peak"
            | "memory.current"
            | "memory.usage_in_bytes"
            | "memory.memsw.usage_in_bytes"
            | "memory.kmem.usage_in_bytes"
            | "memory.events"
            | "memory.events.local"
            | "memory.stat"
            | "io.stat" => Err(VfsError::BadFileDescriptor),
            name if name.starts_with("blkio.")
                && !matches!(
                    name,
                    "blkio.weight"
                        | "blkio.weight_device"
                        | "blkio.throttle.read_bps_device"
                        | "blkio.throttle.write_bps_device"
                        | "blkio.throttle.read_iops_device"
                        | "blkio.throttle.write_iops_device"
                        | "blkio.reset_stats"
                ) =>
            {
                Err(VfsError::BadFileDescriptor)
            }
            name if name.starts_with("hugetlb.")
                && !name.ends_with(".limit_in_bytes")
                && !name.ends_with(".rsvd.limit_in_bytes") =>
            {
                Err(VfsError::BadFileDescriptor)
            }
            _ => {
                let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                let mut value = text.trim_end_matches('\0').trim_end().to_string();
                value.push('\n');
                *self.value.lock() = value;
                Ok(())
            }
        }
    }
}

fn is_read_only_control_file(name: &str) -> bool {
    matches!(
        name,
        "cgroup.controllers"
            | "cgroup.sane_behavior"
            | "cpuset.memory_pressure"
            | "cpu.stat"
            | "cpuacct.usage"
            | "cpuacct.usage_percpu"
            | "cpuacct.stat"
            | "freezer.self_freezing"
            | "freezer.parent_freezing"
            | "devices.list"
            | "net_prio.prioidx"
            | "pids.current"
            | "pids.events"
            | "pids.peak"
            | "memory.current"
            | "memory.usage_in_bytes"
            | "memory.memsw.usage_in_bytes"
            | "memory.kmem.usage_in_bytes"
            | "memory.kmem.max_usage_in_bytes"
            | "memory.events"
            | "memory.events.local"
            | "memory.stat"
            | "io.stat"
    ) || (name.starts_with("blkio.")
        && !matches!(
            name,
            "blkio.weight"
                | "blkio.weight_device"
                | "blkio.throttle.read_bps_device"
                | "blkio.throttle.write_bps_device"
                | "blkio.throttle.read_iops_device"
                | "blkio.throttle.write_iops_device"
                | "blkio.reset_stats"
        ))
        || (name.starts_with("hugetlb.")
            && !name.ends_with(".limit_in_bytes")
            && !name.ends_with(".rsvd.limit_in_bytes"))
}

fn parse_cgroup_u64(data: &[u8]) -> VfsResult<u64> {
    let text = core::str::from_utf8(data)
        .map_err(|_| VfsError::InvalidInput)?
        .trim();
    if text.is_empty() || text.starts_with('-') || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VfsError::InvalidInput);
    }
    text.parse::<u64>().map_err(|_| VfsError::InvalidInput)
}

fn parse_signed_cgroup_i64(data: &[u8]) -> VfsResult<i64> {
    let text = core::str::from_utf8(data)
        .map_err(|_| VfsError::InvalidInput)?
        .trim();
    if text.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VfsError::InvalidInput);
    }
    text.parse::<i64>().map_err(|_| VfsError::InvalidInput)
}

fn parse_classid(data: &[u8]) -> VfsResult<u64> {
    let text = core::str::from_utf8(data)
        .map_err(|_| VfsError::InvalidInput)?
        .trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| VfsError::InvalidInput)
    } else {
        parse_cgroup_u64(data)
    }
}

fn freezer_self_freezing_text(state: &str) -> String {
    if state.trim() == "FROZEN" {
        "1\n".to_string()
    } else {
        "0\n".to_string()
    }
}

fn parse_memory_control_value(data: &[u8]) -> VfsResult<String> {
    let text = core::str::from_utf8(data)
        .map_err(|_| VfsError::InvalidInput)?
        .trim();
    if text == "max" {
        return Ok("max\n".to_string());
    }
    if text == "-1" {
        let unlimited = (i64::MAX as u64 / PAGE_SIZE_4K as u64) * PAGE_SIZE_4K as u64;
        return Ok(format!("{unlimited}\n"));
    }
    let Some(bytes) = parse_size_bytes(text) else {
        return Err(VfsError::InvalidInput);
    };
    let aligned = (bytes / PAGE_SIZE_4K as u64) * PAGE_SIZE_4K as u64;
    Ok(format!("{aligned}\n"))
}

fn parse_size_bytes(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut value = 0u64;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        value = value
            .checked_mul(10)?
            .checked_add((bytes[index] - b'0') as u64)?;
        index += 1;
    }
    if index == 0 {
        return None;
    }
    let multiplier = match &text[index..] {
        "" => 1,
        "K" | "k" => 1024,
        "M" | "m" => 1024 * 1024,
        "G" | "g" => 1024 * 1024 * 1024,
        _ => return None,
    };
    value.checked_mul(multiplier)
}

fn cpuset_topology_mask(count: usize) -> String {
    if count <= 1 {
        "0\n".to_string()
    } else {
        format!("0-{}\n", count - 1)
    }
}

fn cpuset_mask_max(name: &str) -> usize {
    if name.ends_with("mems") {
        CPUSET_NODE_COUNT
    } else {
        axhal::cpu_num().max(1)
    }
}

fn cpuset_exclusive_flag_for_mask(mask_name: &str) -> &'static str {
    if mask_name.ends_with("mems") {
        "cpuset.mem_exclusive"
    } else {
        "cpuset.cpu_exclusive"
    }
}

fn parse_cpuset_mask(data: &[u8], max_count: usize) -> VfsResult<(BTreeSet<u32>, String)> {
    let text = core::str::from_utf8(data)
        .map_err(|_| VfsError::InvalidInput)?
        .trim();
    let mut mask = BTreeSet::new();
    if !text.is_empty() {
        for token in text.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Some((start, end)) = token.split_once('-') {
                if start.is_empty() || end.is_empty() {
                    return Err(VfsError::InvalidInput);
                }
                let start = parse_cpuset_index(start, max_count)?;
                let end = parse_cpuset_index(end, max_count)?;
                if start > end {
                    return Err(VfsError::InvalidInput);
                }
                for index in start..=end {
                    mask.insert(index);
                }
            } else {
                mask.insert(parse_cpuset_index(token, max_count)?);
            }
        }
    }
    Ok((mask.clone(), format!("{}\n", format_cpuset_mask(&mask))))
}

fn parse_cpuset_index(text: &str, max_count: usize) -> VfsResult<u32> {
    if text.starts_with('-') || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VfsError::InvalidInput);
    }
    let value = text.parse::<u32>().map_err(|_| VfsError::InvalidInput)?;
    if value as usize >= max_count {
        return Err(VfsError::InvalidInput);
    }
    Ok(value)
}

fn format_cpuset_mask(mask: &BTreeSet<u32>) -> String {
    let mut ranges = Vec::new();
    let mut iter = mask.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek().is_some_and(|next| *next == end + 1) {
            end = iter.next().unwrap();
        }
        if start == end {
            ranges.push(start.to_string());
        } else {
            ranges.push(format!("{start}-{end}"));
        }
    }
    ranges.join(",")
}

fn masks_overlap(a: &BTreeSet<u32>, b: &BTreeSet<u32>) -> bool {
    a.iter().any(|value| b.contains(value))
}

fn parse_cpuset_flag(data: &[u8]) -> VfsResult<bool> {
    let text = core::str::from_utf8(data)
        .map_err(|_| VfsError::InvalidInput)?
        .trim();
    if text.starts_with('-') || text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(VfsError::InvalidInput);
    }
    let value = text.parse::<u64>().map_err(|_| VfsError::InvalidInput)?;
    Ok(value != 0)
}

fn flag_text(value: bool) -> String {
    if value {
        "1\n".to_string()
    } else {
        "0\n".to_string()
    }
}

fn default_value(name: &str) -> String {
    match name {
        "release_agent" => "\n",
        "cgroup.sane_behavior" => "0\n",
        "cgroup.event_control" => "\n",
        "cpus" | "cpuset.cpus" | "mems" | "cpuset.mems" => "\n",
        "cpu.max" => "max 100000\n",
        "cpu.shares" => "1024\n",
        "cpu.cfs_quota_us" => "-1\n",
        "cpu.cfs_period_us" => "100000\n",
        "cpu.rt_runtime_us" => "950000\n",
        "cpu.rt_period_us" => "1000000\n",
        "cpu.stat" => "nr_periods 0\nnr_throttled 0\nthrottled_time 0\n",
        "cpuacct.usage" | "cpuacct.usage_percpu" => "0\n",
        "cpuacct.stat" => "user 0\nsystem 0\n",
        "freezer.state" => "THAWED\n",
        "freezer.self_freezing" | "freezer.parent_freezing" => "0\n",
        "cpuset.sched_load_balance" => "1\n",
        "cpuset.sched_relax_domain_level" => "-1\n",
        "cpuset.memory_pressure" => "0\n",
        "memory.max"
        | "memory.limit_in_bytes"
        | "memory.memsw.limit_in_bytes"
        | "memory.kmem.limit_in_bytes"
        | "memory.soft_limit_in_bytes"
        | "pids.max" => "max\n",
        "memory.swappiness" => "60\n",
        "memory.oom_control" => "oom_kill_disable 0\nunder_oom 0\noom_kill 0\n",
        "memory.move_charge_at_immigrate" => "0\n",
        "io.max" => "\n",
        "memory.events" | "memory.events.local" | "memory.stat" | "io.stat" => "",
        "blkio.weight" => "500\n",
        "blkio.weight_device"
        | "blkio.throttle.read_bps_device"
        | "blkio.throttle.write_bps_device"
        | "blkio.throttle.read_iops_device"
        | "blkio.throttle.write_iops_device" => "\n",
        "blkio.reset_stats" => "0\n",
        "blkio.throttle.io_serviced"
        | "blkio.throttle.io_service_bytes"
        | "blkio.io_serviced"
        | "blkio.io_service_bytes"
        | "blkio.io_service_time"
        | "blkio.io_wait_time"
        | "blkio.io_merged"
        | "blkio.io_queued"
        | "blkio.sectors"
        | "blkio.time" => "Total 0\n",
        "devices.list" => "a *:* rwm\n",
        "devices.allow" | "devices.deny" => "\n",
        "net_cls.classid" => "0\n",
        "net_prio.ifpriomap" => "\n",
        "net_prio.prioidx" => "0\n",
        name if name.starts_with("hugetlb.") && name.ends_with(".limit_in_bytes") => "max\n",
        name if name.starts_with("hugetlb.") && name.ends_with(".numa_stat") => "total=0 N0=0\n",
        name if name.starts_with("hugetlb.")
            && (name.ends_with(".usage_in_bytes")
                || name.ends_with(".max_usage_in_bytes")
                || name.ends_with(".failcnt")) =>
        {
            "0\n"
        }
        "pids.events" => "max 0\n",
        "pids.peak" => "0\n",
        _ => "0\n",
    }
    .to_string()
}

impl NodeOps for CgroupFile {
    fn inode(&self) -> u64 {
        self.node.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let mut metadata = self.node.metadata();
        metadata.size = self.read_text().map_or(0, |text| text.len() as u64);
        Ok(metadata)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.node.update_metadata(update);
        Ok(())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        self.node.fs.as_ref()
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

impl FileNodeOps for CgroupFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.read_text()?;
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let data = &data.as_bytes()[offset as usize..];
        let len = data.len().min(buf.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok(len)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.write_text(buf)?;
        Ok(buf.len())
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        self.write_text(buf)?;
        Ok((buf.len(), self.read_text()?.len() as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        if len == 0 {
            return Ok(());
        }
        Err(VfsError::InvalidInput)
    }

    fn set_symlink(&self, _target: &str) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
}

impl Pollable for CgroupFile {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

impl CgroupDir {
    fn bind_control_files(self: &Arc<Self>) {
        for file in self.files.values() {
            file.bind_dir(self);
        }
    }
}
