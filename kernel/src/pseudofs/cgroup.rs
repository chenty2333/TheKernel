use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    sync::atomic::{AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DeviceId, DirEntry, DirEntrySink, DirNode, DirNodeOps,
    FileNode, FileNodeOps, Filesystem, FilesystemOps, Metadata, MetadataUpdate, NamedCreateOptions,
    NodeFlags, NodeOps, NodePermission, NodeType, Reference, RenameRequest, StatFs, UnlinkRequest,
    VfsError, VfsResult, WeakDirEntry, path::MAX_NAME_LEN,
};
use axhal::time::wall_time;
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use hashbrown::{HashMap, HashSet};
use spin::Lazy;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};

use super::pseudo_stat_fs;
use crate::{
    file::{Directory, OpenCredentials, current_file_write_credentials, get_typed_file},
    task::{AsThread, get_process_data, get_process_including_zombie, send_signal_to_process},
};

const CGROUP_SUPER_MAGIC: u32 = 0x27e0_eb;
const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;
const MAX_CGROUP_CHILDREN: usize = 65_536;
/// Bound recursive hierarchy walks and the number of simultaneously held
/// descendant locks. Cgroup state is synthetic and starts empty on every boot,
/// so enforcing this at create/move admission covers every reachable tree.
const MAX_CGROUP_DEPTH: usize = 256;
/// TheKernel does not yet have system-wide task accounting that can serve as
/// a cgroup membership budget. Keep both membership indexes explicitly
/// bounded until that accounting can own a tunable limit.
const MAX_CGROUP_MEMBERSHIPS: usize = 65_536;

fn try_reserve_cgroup_child_slot(
    children: &mut HashMap<String, Arc<CgroupDir>>,
    limit: usize,
    grows: bool,
) -> VfsResult<()> {
    if grows && children.len() >= limit {
        return Err(VfsError::NoMemory);
    }
    children.try_reserve(1).map_err(|_| VfsError::NoMemory)
}

fn try_owned(value: &str) -> VfsResult<String> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| VfsError::NoMemory)?;
    result.push_str(value);
    Ok(result)
}

fn try_join_names<'a, I>(names: I) -> VfsResult<String>
where
    I: Iterator<Item = &'a str> + Clone,
{
    let count = names.clone().count();
    let capacity = names
        .clone()
        .try_fold(0usize, |total, name| total.checked_add(name.len()))
        .and_then(|capacity| capacity.checked_add(count.saturating_sub(1)))
        .and_then(|capacity| capacity.checked_add(usize::from(count != 0)))
        .ok_or(VfsError::NoMemory)?;
    let mut out = String::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| VfsError::NoMemory)?;
    for (index, name) in names.enumerate() {
        if index != 0 {
            out.push(' ');
        }
        out.push_str(name);
    }
    if count != 0 {
        out.push('\n');
    }
    Ok(out)
}

const CONTROL_FILES: &[&str] = &[
    "tasks",
    "cgroup.procs",
    "cgroup.controllers",
    "cgroup.subtree_control",
    "cgroup.kill",
    "pids.max",
    "pids.current",
    "pids.events",
    "pids.peak",
];
/// One global synthetic-inode budget for a cgroup filesystem.  Per-parent
/// child limits alone do not bound a deep tree; this keeps allocator-backed
/// identity bookkeeping finite until cgroup memory accounting exists.
const MAX_CGROUP_INODES: usize = (MAX_CGROUP_CHILDREN + 1) * (CONTROL_FILES.len() + 1);

const ALL_CONTROLLERS: &[&str] = &["pids"];
const KNOWN_V1_CONTROLLERS: &[&str] = &[
    "blkio",
    "cpu",
    "cpuacct",
    "cpuset",
    "debug",
    "devices",
    "freezer",
    "hugetlb",
    "memory",
    "misc",
    "net_cls",
    "net_prio",
    "perf_event",
    "pids",
    "rdma",
];

struct PidMembershipRegistry {
    /// Serializes admission and publication. Registry locks are always taken
    /// after this lock, with the global map before per-cgroup member sets.
    operation: Mutex<()>,
    by_pid: Mutex<HashMap<Pid, Weak<CgroupDir>>>,
    global_limit: usize,
    per_cgroup_limit: usize,
}

static PID_CGROUPS: Lazy<PidMembershipRegistry> = Lazy::new(|| {
    PidMembershipRegistry::with_limits(MAX_CGROUP_MEMBERSHIPS, MAX_CGROUP_MEMBERSHIPS)
});

#[derive(Clone, Copy, PartialEq, Eq)]
enum CgroupVersion {
    V1,
    V2,
}

pub fn new_cgroup_v1(controllers: Vec<String>) -> VfsResult<Filesystem> {
    CgroupFs::new(CgroupVersion::V1, controllers)
}

pub fn new_cgroup_v2() -> VfsResult<Filesystem> {
    let mut controllers = Vec::new();
    controllers
        .try_reserve_exact(ALL_CONTROLLERS.len())
        .map_err(|_| VfsError::NoMemory)?;
    for controller in ALL_CONTROLLERS {
        controllers.push(try_owned(controller)?);
    }
    CgroupFs::new(CgroupVersion::V2, controllers)
}

pub fn parse_v1_controllers(source: &str, data: &str) -> AxResult<Vec<String>> {
    let mut controllers = Vec::new();
    for token in source.split(',') {
        let token = token.trim();
        if ALL_CONTROLLERS.contains(&token) && !controllers.iter().any(|it| it == token) {
            controllers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            controllers.push(try_owned(token)?);
        } else if KNOWN_V1_CONTROLLERS.contains(&token) {
            return Err(AxError::NoSuchDevice);
        }
    }
    for token in data.split(',') {
        let token = token.trim();
        if token.is_empty() || is_generic_cgroup_mount_option(token) {
            continue;
        }
        if ALL_CONTROLLERS.contains(&token) && !controllers.iter().any(|it| it == token) {
            controllers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            controllers.push(try_owned(token)?);
        } else if KNOWN_V1_CONTROLLERS.contains(&token) {
            return Err(AxError::NoSuchDevice);
        } else {
            return Err(AxError::InvalidInput);
        }
    }
    if controllers.is_empty() {
        controllers.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        controllers.push(try_owned("pids")?);
    }
    Ok(controllers)
}

fn is_generic_cgroup_mount_option(token: &str) -> bool {
    matches!(
        token,
        "none" | "cgroup" | "rw" | "ro" | "relatime" | "nosuid" | "nodev" | "noexec"
    )
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
    namespace: Mutex<()>,
    inodes: Mutex<HashSet<u64>>,
    next_inode: AtomicU64,
    root: Mutex<Option<DirEntry>>,
    root_dir: Mutex<Option<Arc<CgroupDir>>>,
}

impl CgroupFs {
    fn new(version: CgroupVersion, controllers: Vec<String>) -> VfsResult<Filesystem> {
        let fs = Arc::try_new(Self {
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
            namespace: Mutex::new(()),
            inodes: Mutex::new(HashSet::new()),
            next_inode: AtomicU64::new(1),
            root: Mutex::new(None),
            root_dir: Mutex::new(None),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let filesystem = Filesystem::try_new(fs.clone())?;
        let root_dir = CgroupDir::try_new_root(fs.clone())?;
        let root = DirEntry::try_new_dir(DirNode::new(root_dir.clone()), Reference::root())?;
        root_dir.bind(root.downgrade());
        *fs.root_dir.lock() = Some(root_dir.clone());
        *fs.root.lock() = Some(root);
        Ok(filesystem)
    }

    fn try_alloc_inode(&self) -> VfsResult<u64> {
        let mut inodes = self.inodes.lock();
        if inodes.len() >= MAX_CGROUP_INODES {
            return Err(VfsError::NoMemory);
        }
        inodes.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        let ino = self
            .next_inode
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| VfsError::StorageFull)?;
        inodes.insert(ino);
        Ok(ino)
    }

    fn release_inode(&self, ino: u64) {
        self.inodes.lock().remove(&ino);
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
        Ok(pseudo_stat_fs(self.fs_type))
    }

    fn unmount(&self) {
        self.root.lock().take();
        self.root_dir.lock().take();
    }
}

struct CgroupNode {
    fs: Arc<CgroupFs>,
    ino: u64,
    metadata: Mutex<Metadata>,
}

impl CgroupNode {
    fn try_new(fs: Arc<CgroupFs>, node_type: NodeType, mode: NodePermission) -> VfsResult<Self> {
        let ino = fs.try_alloc_inode()?;
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
        Ok(Self {
            fs,
            ino,
            metadata: Mutex::new(metadata),
        })
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
    parent: Mutex<Option<Weak<CgroupDir>>>,
    this: Mutex<Option<WeakDirEntry>>,
    namespace_epoch: AtomicU64,
    children: Mutex<HashMap<String, Arc<CgroupDir>>>,
    files: HashMap<&'static str, Arc<CgroupFile>>,
    pids: Mutex<HashSet<Pid>>,
    pids_max: Mutex<Option<u64>>,
    pids_peak: Mutex<u64>,
    pids_events_limit: Mutex<u64>,
    subtree_control: Mutex<HashSet<String>>,
}

impl CgroupDir {
    fn try_new_root(fs: Arc<CgroupFs>) -> VfsResult<Arc<Self>> {
        Self::try_new(fs, None)
    }

    fn try_new(fs: Arc<CgroupFs>, parent: Option<Weak<CgroupDir>>) -> VfsResult<Arc<Self>> {
        let mode = NodePermission::from_bits_truncate(0o755);
        let mut files = HashMap::new();
        files
            .try_reserve(CONTROL_FILES.len())
            .map_err(|_| VfsError::NoMemory)?;
        for &name in CONTROL_FILES {
            files.insert(name, CgroupFile::try_new(fs.clone(), name)?);
        }
        let node = CgroupNode::try_new(fs, NodeType::Directory, mode)?;
        let dir = Arc::try_new(Self {
            node,
            parent: Mutex::new(parent),
            this: Mutex::new(None),
            namespace_epoch: AtomicU64::new(0),
            children: Mutex::new(HashMap::new()),
            files,
            pids: Mutex::new(HashSet::new()),
            pids_max: Mutex::new(None),
            pids_peak: Mutex::new(0),
            pids_events_limit: Mutex::new(0),
            subtree_control: Mutex::new(HashSet::new()),
        })
        .map_err(|_| VfsError::NoMemory)?;
        dir.bind_control_files();
        Ok(dir)
    }

    fn bind(&self, this: WeakDirEntry) {
        *self.this.lock() = Some(this);
    }

    fn reference(&self, name: &str) -> VfsResult<Reference> {
        Ok(Reference::new(
            self.this.lock().as_ref().and_then(WeakDirEntry::upgrade),
            try_owned(name)?,
        ))
    }

    fn try_child_entry(&self, name: &str, child: Arc<CgroupDir>) -> VfsResult<DirEntry> {
        let entry = DirEntry::try_new_dir(DirNode::new(child.clone()), self.reference(name)?)?;
        child.bind(entry.downgrade());
        Ok(entry)
    }

    fn try_file_entry(&self, name: &str, file: Arc<CgroupFile>) -> VfsResult<DirEntry> {
        DirEntry::try_new_file(
            FileNode::new(file),
            NodeType::RegularFile,
            self.reference(name)?,
        )
    }

    fn matches_expected_dir(&self, expected: &DirEntry, actual: &Arc<CgroupDir>) -> bool {
        expected.downcast::<CgroupDir>().is_ok_and(|expected| {
            Arc::ptr_eq(&self.node.fs, &expected.node.fs) && Arc::ptr_eq(&expected, actual)
        })
    }

    fn touch_namespace(&self, now: core::time::Duration) {
        self.node.update_metadata(MetadataUpdate {
            mtime: Some(now),
            ctime: Some(now),
            ..Default::default()
        });
    }

    fn is_same_or_descendant_of(candidate: &Arc<Self>, ancestor: &Arc<Self>) -> bool {
        let mut current = Some(candidate.clone());
        for _ in 0..=MAX_CGROUP_DEPTH {
            let Some(dir) = current else {
                return false;
            };
            if Arc::ptr_eq(&dir, ancestor) {
                return true;
            }
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
        // A hierarchy deeper than the admitted bound, or a parent cycle, is
        // malformed. Conservatively reject moves through it.
        true
    }

    fn hierarchy_depth(&self) -> VfsResult<usize> {
        let mut depth = 0usize;
        let mut current = self.parent.lock().as_ref().and_then(Weak::upgrade);
        while let Some(dir) = current {
            depth = depth.checked_add(1).ok_or(VfsError::FilesystemLoop)?;
            if depth > MAX_CGROUP_DEPTH {
                return Err(VfsError::FilesystemLoop);
            }
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
        Ok(depth)
    }

    fn subtree_height(&self, remaining: usize) -> VfsResult<usize> {
        let children = self.children.lock();
        if children.is_empty() {
            return Ok(0);
        }
        if remaining == 0 {
            return Err(VfsError::FilesystemLoop);
        }
        let mut height = 0usize;
        for child in children.values() {
            height = height.max(
                child
                    .subtree_height(remaining - 1)?
                    .checked_add(1)
                    .ok_or(VfsError::FilesystemLoop)?,
            );
        }
        Ok(height)
    }

    fn try_live_pids(&self) -> VfsResult<Vec<Pid>> {
        let pids = self.pids.lock();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(pids.len())
            .map_err(|_| VfsError::NoMemory)?;
        snapshot.extend(pids.iter().copied());
        Ok(snapshot)
    }

    fn recursive_live_pid_count(&self) -> usize {
        let local = self.pids.lock().len();
        local
            + self
                .children
                .lock()
                .values()
                .map(|child| child.recursive_live_pid_count())
                .sum::<usize>()
    }

    fn update_pids_peak(&self, count: usize) {
        if !self.pids_controller_active() {
            return;
        }
        let mut peak = self.pids_peak.lock();
        *peak = (*peak).max(count as u64);
    }

    fn update_pids_peak_hierarchy(self: &Arc<Self>) {
        let mut current = Some(self.clone());
        while let Some(dir) = current {
            dir.update_pids_peak(dir.recursive_live_pid_count());
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
    }

    fn limiting_dir_for_fork(self: &Arc<Self>) -> Option<Arc<CgroupDir>> {
        let mut current = Some(self.clone());
        while let Some(dir) = current {
            if dir.pids_controller_active() {
                let limit = *dir.pids_max.lock();
                if let Some(limit) = limit
                    && dir.recursive_live_pid_count() as u64 + 1 > limit
                {
                    return Some(dir);
                }
            }
            current = dir.parent.lock().as_ref().and_then(Weak::upgrade);
        }
        None
    }

    fn has_real_children(&self) -> bool {
        !self.children.lock().is_empty()
    }

    fn pids_controller_active(&self) -> bool {
        match self.node.fs.version {
            CgroupVersion::V1 => self
                .node
                .fs
                .controllers
                .iter()
                .any(|controller| controller == "pids"),
            CgroupVersion::V2 => self
                .parent
                .lock()
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|parent| parent.subtree_control.lock().contains("pids")),
        }
    }

    fn control_file_visible(&self, name: &str) -> bool {
        match self.node.fs.version {
            CgroupVersion::V1 => {
                matches!(name, "tasks" | "cgroup.procs")
                    || (name.starts_with("pids.") && self.pids_controller_active())
            }
            CgroupVersion::V2 => match name {
                "cgroup.procs" | "cgroup.controllers" | "cgroup.subtree_control" => true,
                "cgroup.kill" => self.parent.lock().is_some(),
                _ if name.starts_with("pids.") => self.pids_controller_active(),
                _ => false,
            },
        }
    }

    fn reset_pids_controller(&self) {
        *self.pids_max.lock() = None;
        *self.pids_peak.lock() = 0;
        *self.pids_events_limit.lock() = 0;
    }

    fn initialize_pids_controller(&self) {
        self.reset_pids_controller();
        self.update_pids_peak(self.recursive_live_pid_count());
    }

    fn v2_has_enabled_child_controllers(&self) -> bool {
        self.node.fs.version == CgroupVersion::V2
            && self.parent.lock().is_some()
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
        let this = self.this_dir()?;
        PID_CGROUPS.try_attach(&this, pid, false)?;
        Ok(())
    }

    fn attach_fork_child(self: &Arc<Self>, pid: Pid) -> AxResult<()> {
        PID_CGROUPS.try_attach(self, pid, true)?;
        Ok(())
    }

    fn kill_attached(&self) -> VfsResult<()> {
        for pid in self.try_live_pids()? {
            let _ = send_signal_to_process(pid, Some(SignalInfo::new_kernel(Signo::SIGKILL)));
        }
        Ok(())
    }

    fn kill_attached_recursive(&self) -> VfsResult<()> {
        self.kill_attached()?;
        for child in self.children.lock().values() {
            child.kill_attached_recursive()?;
        }
        Ok(())
    }

    fn tasks_text(&self) -> VfsResult<String> {
        let pids = self.pids.lock();
        let mut out = String::new();
        out.try_reserve_exact(pids.len().saturating_mul(22))
            .map_err(|_| VfsError::NoMemory)?;
        for pid in pids.iter() {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{pid}\n"));
        }
        Ok(out)
    }

    fn subtree_control_text(&self) -> VfsResult<String> {
        let control = self.subtree_control.lock();
        try_join_names(control.iter().map(String::as_str))
    }

    fn controller_available(&self, name: &str) -> bool {
        if self.node.fs.version == CgroupVersion::V1 {
            return false;
        }
        if let Some(parent) = self.parent.lock().as_ref().and_then(Weak::upgrade) {
            return parent.subtree_control.lock().contains(name);
        }
        self.node
            .fs
            .controllers
            .iter()
            .any(|controller| controller == name)
    }

    fn controllers_text(&self) -> VfsResult<String> {
        if self.node.fs.version == CgroupVersion::V1 {
            return Ok(String::new());
        }
        if let Some(parent) = self.parent.lock().as_ref().and_then(Weak::upgrade) {
            let control = parent.subtree_control.lock();
            return try_join_names(control.iter().map(String::as_str));
        }
        try_join_names(self.node.fs.controllers.iter().map(String::as_str))
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
        // `pids` is currently the only implemented controller. Track the last
        // requested state directly instead of allocating transient sets and a
        // cloned replacement tree for every write.
        let mut pids_state = None;
        for token in text.split_ascii_whitespace() {
            if token.len() < 2 {
                return Err(VfsError::InvalidInput);
            }
            let (op, name) = token.split_at(1);
            match op {
                "+" => {
                    if !self.controller_available(name) {
                        return Err(VfsError::NotFound);
                    }
                    if name != "pids" {
                        return Err(VfsError::OperationNotSupported);
                    }
                    pids_state = Some(true);
                }
                "-" => {
                    if name == "pids" {
                        pids_state = Some(false);
                    }
                }
                _ => return Err(VfsError::InvalidInput),
            }
        }

        let Some(enable_pids) = pids_state else {
            return Ok(());
        };
        let prepared_name = enable_pids.then(|| try_owned("pids")).transpose()?;
        let mut control = self.subtree_control.lock();
        let was_enabled = control.contains("pids");
        if enable_pids == was_enabled {
            return Ok(());
        }
        if enable_pids
            && self.node.fs.version == CgroupVersion::V2
            && self.parent.lock().is_some()
            && !self.pids.lock().is_empty()
        {
            return Err(VfsError::ResourceBusy);
        }
        if !enable_pids && self.child_has_subtree_controller("pids") {
            return Err(VfsError::ResourceBusy);
        }
        if enable_pids {
            control.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            control.insert(prepared_name.ok_or(VfsError::Io)?);
        } else {
            control.remove("pids");
        }
        drop(control);
        for child in self.children.lock().values() {
            if enable_pids {
                child.initialize_pids_controller();
            } else {
                child.reset_pids_controller();
            }
        }
        Ok(())
    }
}

fn same_membership_mapping(lhs: Option<&Weak<CgroupDir>>, rhs: Option<&Weak<CgroupDir>>) -> bool {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => Weak::ptr_eq(lhs, rhs),
        (None, None) => true,
        _ => false,
    }
}

impl PidMembershipRegistry {
    fn with_limits(global_limit: usize, per_cgroup_limit: usize) -> Self {
        Self {
            operation: Mutex::new(()),
            by_pid: Mutex::new(HashMap::new()),
            global_limit,
            per_cgroup_limit,
        }
    }

    fn try_attach(&self, target: &Arc<CgroupDir>, pid: Pid, charge_fork: bool) -> AxResult<bool> {
        let _operation = self.operation.lock();
        self.try_attach_locked(target, pid, charge_fork)
    }

    fn try_charge_from(&self, parent_pid: Pid, child_pid: Pid) -> AxResult<()> {
        let _operation = self.operation.lock();
        let target = {
            let mut by_pid = self.by_pid.lock();
            let Some(mapped) = by_pid.get(&parent_pid).cloned() else {
                return Ok(());
            };
            let Some(target) = mapped.upgrade() else {
                by_pid.remove(&parent_pid);
                return Ok(());
            };
            target
        };
        self.try_attach_locked(&target, child_pid, true)?;
        Ok(())
    }

    /// Reserves both indexes before changing either one. The operation lock
    /// prevents another writer from consuming those reservations; publication
    /// then holds the global map and every affected member set, so readers can
    /// observe only the old state or the fully committed new state.
    fn try_attach_locked(
        &self,
        target: &Arc<CgroupDir>,
        pid: Pid,
        charge_fork: bool,
    ) -> AxResult<bool> {
        let target_weak = Arc::downgrade(target);
        let old_mapping = self.by_pid.lock().get(&pid).cloned();
        let target_had_pid = target.pids.lock().contains(&pid);
        if target_had_pid
            && old_mapping
                .as_ref()
                .is_some_and(|old| Weak::ptr_eq(old, &target_weak))
        {
            return Ok(false);
        }

        if charge_fork
            && !target_had_pid
            && let Some(limiting) = target.limiting_dir_for_fork()
        {
            *limiting.pids_events_limit.lock() += 1;
            return Err(AxError::WouldBlock);
        }

        if !target_had_pid {
            let mut target_pids = target.pids.lock();
            if target_pids.len() >= self.per_cgroup_limit {
                return Err(AxError::NoMemory);
            }
            target_pids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        }
        if old_mapping.is_none() {
            let mut by_pid = self.by_pid.lock();
            by_pid.retain(|_, mapped| mapped.strong_count() != 0);
            if by_pid.len() >= self.global_limit {
                return Err(AxError::NoMemory);
            }
            by_pid.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        }

        let mut by_pid = self.by_pid.lock();
        if !same_membership_mapping(by_pid.get(&pid), old_mapping.as_ref()) {
            return Err(AxError::Io);
        }
        let old_dir = old_mapping
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|old| !Arc::ptr_eq(old, target));

        if let Some(old_dir) = old_dir {
            let mut old_pids = old_dir.pids.lock();
            let mut target_pids = target.pids.lock();
            if target_pids.contains(&pid) != target_had_pid {
                return Err(AxError::Io);
            }
            let inserted_target = if target_had_pid {
                false
            } else {
                target_pids.insert(pid)
            };
            if !target_had_pid && !inserted_target {
                return Err(AxError::Io);
            }
            let removed_old = old_pids.remove(&pid);
            let replaced = by_pid.insert(pid, target_weak);
            if !same_membership_mapping(replaced.as_ref(), old_mapping.as_ref()) {
                if let Some(old_mapping) = old_mapping {
                    by_pid.insert(pid, old_mapping);
                } else {
                    by_pid.remove(&pid);
                }
                if inserted_target {
                    target_pids.remove(&pid);
                }
                if removed_old {
                    old_pids.insert(pid);
                }
                return Err(AxError::Io);
            }
        } else {
            let mut target_pids = target.pids.lock();
            if target_pids.contains(&pid) != target_had_pid {
                return Err(AxError::Io);
            }
            let inserted_target = if target_had_pid {
                false
            } else {
                target_pids.insert(pid)
            };
            if !target_had_pid && !inserted_target {
                return Err(AxError::Io);
            }
            let replaced = by_pid.insert(pid, target_weak);
            if !same_membership_mapping(replaced.as_ref(), old_mapping.as_ref()) {
                if let Some(old_mapping) = old_mapping {
                    by_pid.insert(pid, old_mapping);
                } else {
                    by_pid.remove(&pid);
                }
                if inserted_target {
                    target_pids.remove(&pid);
                }
                return Err(AxError::Io);
            }
        }

        drop(by_pid);
        target.update_pids_peak_hierarchy();
        Ok(true)
    }

    fn detach(&self, pid: Pid) {
        let _operation = self.operation.lock();
        let mut by_pid = self.by_pid.lock();
        let old = by_pid.remove(&pid).and_then(|weak| weak.upgrade());
        if let Some(dir) = old {
            dir.pids.lock().remove(&pid);
        }
    }

    fn get(&self, pid: Pid) -> Option<Arc<CgroupDir>> {
        let _operation = self.operation.lock();
        let mut by_pid = self.by_pid.lock();
        let mapped = by_pid.get(&pid).cloned()?;
        let Some(dir) = mapped.upgrade() else {
            by_pid.remove(&pid);
            return None;
        };
        Some(dir)
    }
}

fn can_migrate_with_credentials(
    credentials: OpenCredentials,
    target: &crate::task::ProcessData,
) -> bool {
    let target_ids = target.current_cred().ids();
    credentials.euid == 0
        || credentials.fsuid == 0
        || [
            credentials.uid,
            credentials.euid,
            credentials.suid,
            credentials.fsuid,
        ]
        .into_iter()
        .any(|uid| uid == target_ids.ruid || uid == target_ids.euid || uid == target_ids.suid)
}

fn can_migrate_from_open_cgroup_namespace(credentials: OpenCredentials) -> bool {
    credentials.cgroup_ns_id == 0
        || credentials.cgroup_ns_id == axtask::current().as_thread().proc_data.cgroup_ns_id()
}

fn detach_mapped_pid(pid: Pid) {
    PID_CGROUPS.detach(pid);
}

pub(crate) fn detach_process(pid: Pid) {
    detach_mapped_pid(pid);
}

fn cgroup_for_pid(pid: Pid) -> Option<Arc<CgroupDir>> {
    let dir = PID_CGROUPS.get(pid)?;
    if get_process_including_zombie(pid).is_err() {
        PID_CGROUPS.detach(pid);
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

pub fn try_charge_fork(parent_pid: Pid, child_pid: Pid) -> AxResult<()> {
    PID_CGROUPS.try_charge_from(parent_pid, child_pid)
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
    dir.attach_fork_child(child_pid)
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
    fn namespace_epoch(&self) -> u64 {
        self.namespace_epoch.load(Ordering::Acquire)
    }

    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let parent_ino = self
            .parent
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .map_or(self.node.ino, |parent| parent.node.ino);
        let mut position = 0_u64;
        let mut count = 0;
        let mut emit = |name: &str, ino: u64, node_type: NodeType| {
            let current = position;
            position = position.saturating_add(1);
            if current < offset {
                return true;
            }
            if !sink.accept(name, ino, node_type, position) {
                return false;
            }
            count += 1;
            true
        };
        if !emit(".", self.node.ino, NodeType::Directory)
            || !emit("..", parent_ino, NodeType::Directory)
        {
            return Ok(count);
        }
        for (name, dir) in self.children.lock().iter() {
            if !emit(name, dir.node.ino, NodeType::Directory) {
                return Ok(count);
            }
        }
        for (name, file) in &self.files {
            if self.control_file_visible(name) && !emit(name, file.node.ino, NodeType::RegularFile)
            {
                return Ok(count);
            }
        }
        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        if let Some(child) = self.children.lock().get(name).cloned() {
            return self.try_child_entry(name, child);
        }
        if self.control_file_visible(name)
            && let Some(file) = self.files.get(name).cloned()
        {
            return self.try_file_entry(name, file);
        }
        Err(VfsError::NotFound)
    }

    fn create_named(
        &self,
        name: &str,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if name.contains('\n') {
            return Err(VfsError::InvalidInput);
        }
        let _namespace = self.node.fs.namespace.lock();
        let mut children = self.children.lock();
        if let Some(child) = children.get(name).cloned() {
            if disposition == CreateDisposition::Exclusive {
                return Err(VfsError::AlreadyExists);
            }
            return Ok(CreateOutcome {
                entry: self.try_child_entry(name, child)?,
                created: false,
            });
        }
        if self.files.contains_key(name) {
            if disposition == CreateDisposition::Exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let file = self
                .control_file_visible(name)
                .then(|| self.files.get(name).cloned())
                .flatten()
                .ok_or(VfsError::NotFound)?;
            return Ok(CreateOutcome {
                entry: self.try_file_entry(name, file)?,
                created: false,
            });
        }
        if options.node_type != NodeType::Directory || options.rdev.is_some() {
            return Err(VfsError::OperationNotPermitted);
        }
        if self.hierarchy_depth()? >= MAX_CGROUP_DEPTH {
            return Err(VfsError::FilesystemLoop);
        }
        try_reserve_cgroup_child_slot(&mut children, MAX_CGROUP_CHILDREN, true)?;
        let owned_name = try_owned(name)?;
        let child = Self::try_new(
            self.node.fs.clone(),
            Some(Arc::downgrade(&self.this_dir()?)),
        )?;
        child.node.update_metadata(MetadataUpdate {
            mode: Some(options.permission),
            owner: options.owner,
            ..Default::default()
        });
        let entry = self.try_child_entry(name, child.clone())?;
        options.install_initial_data(&entry)?;
        self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        children.insert(owned_name, child.clone());
        let now = wall_time();
        drop(children);
        self.touch_namespace(now);
        Ok(CreateOutcome {
            entry,
            created: true,
        })
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        let _namespace = self.node.fs.namespace.lock();
        if self.files.contains_key(request.name) {
            return Err(VfsError::OperationNotPermitted);
        }
        let mut children = self.children.lock();
        let Some(child) = children.get(request.name).cloned() else {
            return Err(VfsError::NotFound);
        };
        if request
            .expected
            .is_some_and(|expected| !self.matches_expected_dir(expected, &child))
        {
            return Err(VfsError::NotFound);
        }
        if !request.is_dir {
            return Err(VfsError::IsADirectory);
        }
        if child.has_real_children() {
            return Err(VfsError::DirectoryNotEmpty);
        }
        if !child.pids.lock().is_empty() {
            return Err(VfsError::ResourceBusy);
        }
        self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
        children.remove(request.name);
        let now = wall_time();
        drop(children);
        self.touch_namespace(now);
        Ok(())
    }

    fn rename(&self, request: RenameRequest<'_>) -> VfsResult<()> {
        let dst_dir = request.dst_dir.downcast::<Self>()?;
        if !Arc::ptr_eq(&self.node.fs, &dst_dir.node.fs) {
            return Err(VfsError::CrossesDevices);
        }
        if self.node.fs.version != CgroupVersion::V1 {
            return Err(VfsError::OperationNotPermitted);
        }
        if request.dst_name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if request.dst_name.contains('\n') {
            return Err(VfsError::InvalidInput);
        }
        let _namespace = self.node.fs.namespace.lock();
        if self.files.contains_key(request.src_name) || dst_dir.files.contains_key(request.dst_name)
        {
            return Err(VfsError::OperationNotPermitted);
        }
        let same_parent = core::ptr::eq(self, Arc::as_ref(&dst_dir));

        if same_parent {
            let mut children = self.children.lock();
            let child = children
                .get(request.src_name)
                .cloned()
                .ok_or(VfsError::NotFound)?;
            if !self.matches_expected_dir(request.src, &child) {
                return Err(VfsError::NotFound);
            }
            let dst = children.get(request.dst_name).cloned();
            match (request.dst, dst.as_ref()) {
                (None, None) => {}
                (Some(expected), Some(actual)) if self.matches_expected_dir(expected, actual) => {}
                _ => return Err(VfsError::NotFound),
            }
            if dst.as_ref().is_some_and(|dst| Arc::ptr_eq(&child, dst)) {
                return Ok(());
            }
            if dst.is_some() {
                return Err(VfsError::AlreadyExists);
            }

            try_reserve_cgroup_child_slot(&mut children, MAX_CGROUP_CHILDREN, false)?;
            let dst_name = try_owned(request.dst_name)?;
            self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
            children.remove(request.src_name);
            children.insert(dst_name, child);
            let now = wall_time();
            drop(children);
            self.touch_namespace(now);
            return Ok(());
        }

        let src_dir = self.this_dir()?;
        let src_is_ancestor = Self::is_same_or_descendant_of(&dst_dir, &src_dir);
        let dst_is_ancestor = Self::is_same_or_descendant_of(&src_dir, &dst_dir);
        let lock_src_first = if src_is_ancestor {
            true
        } else if dst_is_ancestor {
            false
        } else {
            (Arc::as_ptr(&src_dir).cast::<()>() as usize)
                < Arc::as_ptr(&dst_dir).cast::<()>() as usize
        };
        let commit = |src_children: &mut HashMap<String, Arc<CgroupDir>>,
                      dst_children: &mut HashMap<String, Arc<CgroupDir>>|
         -> VfsResult<(Arc<CgroupDir>, bool)> {
            let child = src_children
                .get(request.src_name)
                .cloned()
                .ok_or(VfsError::NotFound)?;
            if !self.matches_expected_dir(request.src, &child) {
                return Err(VfsError::NotFound);
            }
            let dst = dst_children.get(request.dst_name).cloned();
            match (request.dst, dst.as_ref()) {
                (None, None) => {}
                (Some(expected), Some(actual)) if self.matches_expected_dir(expected, actual) => {}
                _ => return Err(VfsError::NotFound),
            }
            if dst.as_ref().is_some_and(|dst| Arc::ptr_eq(&child, dst)) {
                return Ok((child, false));
            }
            if dst.is_some() {
                return Err(VfsError::AlreadyExists);
            }
            if Self::is_same_or_descendant_of(&dst_dir, &child) {
                return Err(VfsError::InvalidInput);
            }
            let target_depth = dst_dir.hierarchy_depth()?;
            let subtree_height = child.subtree_height(MAX_CGROUP_DEPTH)?;
            if target_depth
                .checked_add(1)
                .and_then(|depth| depth.checked_add(subtree_height))
                .is_none_or(|depth| depth > MAX_CGROUP_DEPTH)
            {
                return Err(VfsError::FilesystemLoop);
            }

            try_reserve_cgroup_child_slot(dst_children, MAX_CGROUP_CHILDREN, true)?;
            let dst_name = try_owned(request.dst_name)?;
            let new_parent = Arc::downgrade(&dst_dir);
            self.namespace_epoch.fetch_add(1, Ordering::AcqRel);
            dst_dir.namespace_epoch.fetch_add(1, Ordering::AcqRel);
            src_children.remove(request.src_name);
            dst_children.insert(dst_name, child.clone());
            *child.parent.lock() = Some(new_parent);
            Ok((child, true))
        };
        let (child, changed) = if lock_src_first {
            let mut src_children = self.children.lock();
            let mut dst_children = dst_dir.children.lock();
            commit(&mut src_children, &mut dst_children)?
        } else {
            let mut dst_children = dst_dir.children.lock();
            let mut src_children = self.children.lock();
            commit(&mut src_children, &mut dst_children)?
        };
        if !changed {
            return Ok(());
        }
        let now = wall_time();
        self.touch_namespace(now);
        dst_dir.touch_namespace(now);
        child.update_pids_peak_hierarchy();
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
    dir: Mutex<Option<Weak<CgroupDir>>>,
}

impl CgroupFile {
    fn try_new(fs: Arc<CgroupFs>, name: &'static str) -> VfsResult<Arc<Self>> {
        let mode = NodePermission::from_bits_truncate(match name {
            "cgroup.kill" => 0o200,
            _ if is_read_only_control_file(name) => 0o444,
            _ => 0o644,
        });
        let node = CgroupNode::try_new(fs, NodeType::RegularFile, mode)?;
        Arc::try_new(Self {
            node,
            name,
            dir: Mutex::new(None),
        })
        .map_err(|_| VfsError::NoMemory)
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
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
        Ok(match self.name {
            "tasks" | "cgroup.procs" => dir.tasks_text()?,
            "cgroup.controllers" => dir.controllers_text()?,
            "cgroup.subtree_control" => dir.subtree_control_text()?,
            "cgroup.kill" => return Err(VfsError::BadFileDescriptor),
            "pids.max" => dir.pids_max_text(),
            "pids.current" => format!("{}\n", dir.recursive_live_pid_count()),
            "pids.events" => format!("max {}\n", *dir.pids_events_limit.lock()),
            "pids.peak" => format!("{}\n", *dir.pids_peak.lock()),
            _ => return Err(VfsError::NotFound),
        })
    }

    fn write_text(&self, data: &[u8]) -> VfsResult<()> {
        let dir = self.dir()?;
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
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
                dir.kill_attached_recursive()
            }
            "cgroup.subtree_control" => dir.update_subtree_control(data),
            "pids.max" => dir.set_pids_max(data),
            "cgroup.controllers" | "pids.current" | "pids.events" | "pids.peak" => {
                Err(VfsError::BadFileDescriptor)
            }
            _ => Err(VfsError::NotFound),
        }
    }
}

fn is_read_only_control_file(name: &str) -> bool {
    matches!(
        name,
        "cgroup.controllers" | "pids.current" | "pids.events" | "pids.peak"
    )
}

impl NodeOps for CgroupFile {
    fn inode(&self) -> u64 {
        self.node.ino
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let dir = self.dir()?;
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
        let mut metadata = self.node.metadata();
        metadata.size = self.read_text().map_or(0, |text| text.len() as u64);
        Ok(metadata)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        let dir = self.dir()?;
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
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
        Ok((buf.len(), 0))
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
        if self.name == "cgroup.kill" {
            IoEvents::OUT
        } else if is_read_only_control_file(self.name) {
            IoEvents::IN
        } else {
            IoEvents::IN | IoEvents::OUT
        }
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cgroup_dir() -> Arc<CgroupDir> {
        let fs = Arc::new(CgroupFs {
            name: "test-cgroup",
            fs_type: CGROUP_SUPER_MAGIC,
            version: CgroupVersion::V1,
            controllers: Vec::from(["pids".to_string()]),
            namespace: Mutex::new(()),
            inodes: Mutex::new(HashSet::new()),
            next_inode: AtomicU64::new(1),
            root: Mutex::new(None),
            root_dir: Mutex::new(None),
        });
        CgroupDir::try_new_root(fs).unwrap()
    }

    fn test_cgroup_fs() -> Filesystem {
        CgroupFs::new(CgroupVersion::V1, Vec::from(["pids".to_string()])).unwrap()
    }

    fn maps_to(registry: &PidMembershipRegistry, pid: Pid, expected: &Arc<CgroupDir>) -> bool {
        registry
            .by_pid
            .lock()
            .get(&pid)
            .and_then(Weak::upgrade)
            .is_some_and(|actual| Arc::ptr_eq(&actual, expected))
    }

    #[test]
    fn v1_controller_parser_rejects_unimplemented_controllers() {
        assert_eq!(
            parse_v1_controllers("none", "pids").unwrap(),
            ["pids".to_string()]
        );
        assert_eq!(
            parse_v1_controllers("memory", "").unwrap_err(),
            AxError::NoSuchDevice
        );
        assert_eq!(
            parse_v1_controllers("none", "pids,memory").unwrap_err(),
            AxError::NoSuchDevice
        );
        assert_eq!(
            parse_v1_controllers("none", "unknown").unwrap_err(),
            AxError::InvalidInput
        );
    }

    #[test]
    fn membership_publish_updates_both_indexes() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let target = test_cgroup_dir();

        assert_eq!(registry.try_attach(&target, 101, false), Ok(true));
        assert!(target.pids.lock().contains(&101));
        assert!(maps_to(&registry, 101, &target));
        assert_eq!(*target.pids_peak.lock(), 1);
    }

    #[test]
    fn target_limit_failure_preserves_old_membership() {
        let registry = PidMembershipRegistry::with_limits(4, 1);
        let old = test_cgroup_dir();
        let target = test_cgroup_dir();
        registry.try_attach(&old, 101, false).unwrap();
        registry.try_attach(&target, 202, false).unwrap();

        assert_eq!(
            registry.try_attach(&target, 101, false),
            Err(AxError::NoMemory)
        );
        assert!(old.pids.lock().contains(&101));
        assert!(!target.pids.lock().contains(&101));
        assert!(target.pids.lock().contains(&202));
        assert!(maps_to(&registry, 101, &old));
        assert!(maps_to(&registry, 202, &target));
    }

    #[test]
    fn global_limit_failure_does_not_publish_target_membership() {
        let registry = PidMembershipRegistry::with_limits(1, 4);
        let old = test_cgroup_dir();
        let target = test_cgroup_dir();
        registry.try_attach(&old, 101, false).unwrap();

        assert_eq!(
            registry.try_attach(&target, 202, false),
            Err(AxError::NoMemory)
        );
        assert!(!target.pids.lock().contains(&202));
        assert!(maps_to(&registry, 101, &old));
        assert_eq!(registry.by_pid.lock().len(), 1);
    }

    #[test]
    fn migration_is_atomic_and_same_target_attach_is_idempotent() {
        let registry = PidMembershipRegistry::with_limits(4, 4);
        let old = test_cgroup_dir();
        let target = test_cgroup_dir();
        registry.try_attach(&old, 101, false).unwrap();

        assert_eq!(registry.try_attach(&target, 101, false), Ok(true));
        assert!(!old.pids.lock().contains(&101));
        assert!(target.pids.lock().contains(&101));
        assert!(maps_to(&registry, 101, &target));
        assert_eq!(registry.by_pid.lock().len(), 1);

        assert_eq!(registry.try_attach(&target, 101, false), Ok(false));
        assert_eq!(target.pids.lock().len(), 1);
        assert_eq!(registry.by_pid.lock().len(), 1);
    }

    #[test]
    fn fork_admission_failure_does_not_publish_or_update_counters() {
        let registry = PidMembershipRegistry::with_limits(0, 1);
        let target = test_cgroup_dir();

        assert_eq!(
            registry.try_attach(&target, 101, true),
            Err(AxError::NoMemory)
        );
        assert!(target.pids.lock().is_empty());
        assert!(registry.by_pid.lock().is_empty());
        assert_eq!(*target.pids_peak.lock(), 0);
        assert_eq!(*target.pids_events_limit.lock(), 0);
    }

    #[test]
    fn pids_max_rejection_increments_limit_event_once() {
        let registry = PidMembershipRegistry::with_limits(1, 1);
        let target = test_cgroup_dir();
        *target.pids_max.lock() = Some(0);

        assert_eq!(
            registry.try_attach(&target, 101, true),
            Err(AxError::WouldBlock)
        );
        assert!(target.pids.lock().is_empty());
        assert!(registry.by_pid.lock().is_empty());
        assert_eq!(*target.pids_peak.lock(), 0);
        assert_eq!(*target.pids_events_limit.lock(), 1);
    }

    #[test]
    fn child_slot_limit_rejects_growth_but_allows_same_map_rename_admission() {
        let mut children = HashMap::new();
        let child = test_cgroup_dir();
        try_reserve_cgroup_child_slot(&mut children, 1, true).unwrap();
        children.insert("child".to_string(), child);

        assert_eq!(
            try_reserve_cgroup_child_slot(&mut children, 1, true),
            Err(VfsError::NoMemory)
        );
        assert_eq!(
            try_reserve_cgroup_child_slot(&mut children, 1, false),
            Ok(())
        );
        assert_eq!(children.len(), 1);
    }

    #[test]
    fn cgroup_rename_preserves_identity_and_updates_cross_parent_membership() {
        let fs = test_cgroup_fs();
        let root = fs.root_dir();
        let root_dir = root.as_dir().unwrap();
        let mode = NodePermission::from_bits_truncate(0o755);
        let src_parent = root_dir
            .create("src-parent", NodeType::Directory, mode)
            .unwrap();
        let dst_parent = root_dir
            .create("dst-parent", NodeType::Directory, mode)
            .unwrap();
        let src_dir = src_parent.as_dir().unwrap();
        let dst_dir = dst_parent.as_dir().unwrap();
        let child = src_dir.create("child", NodeType::Directory, mode).unwrap();
        let wrong = src_dir.create("wrong", NodeType::Directory, mode).unwrap();
        let src_backend = src_parent.downcast::<CgroupDir>().unwrap();
        let dst_backend = dst_parent.downcast::<CgroupDir>().unwrap();
        let src_epoch = src_backend.namespace_epoch();
        let dst_epoch = dst_backend.namespace_epoch();

        assert_eq!(
            src_dir
                .rename("child", &wrong, dst_dir, "moved", None)
                .unwrap_err(),
            VfsError::NotFound
        );
        assert_eq!(src_backend.namespace_epoch(), src_epoch);
        assert_eq!(dst_backend.namespace_epoch(), dst_epoch);
        assert_eq!(src_dir.lookup("child").unwrap().inode(), child.inode());
        assert_eq!(dst_dir.lookup("moved").unwrap_err(), VfsError::NotFound);

        src_dir
            .rename("child", &child, dst_dir, "moved", None)
            .unwrap();
        assert_eq!(src_dir.lookup("child").unwrap_err(), VfsError::NotFound);
        let moved = dst_dir.lookup("moved").unwrap();
        assert_eq!(moved.inode(), child.inode());
        let moved_backend = moved.downcast::<CgroupDir>().unwrap();
        let parent = moved_backend
            .parent
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
            .unwrap();
        assert!(Arc::ptr_eq(&parent, &dst_backend));
        assert_eq!(src_backend.namespace_epoch(), src_epoch + 1);
        assert_eq!(dst_backend.namespace_epoch(), dst_epoch + 1);

        let no_op_epoch = dst_backend.namespace_epoch();
        dst_dir
            .rename("moved", &moved, dst_dir, "moved", Some(&moved))
            .unwrap();
        assert_eq!(dst_backend.namespace_epoch(), no_op_epoch);
        assert_eq!(dst_dir.lookup("moved").unwrap().inode(), child.inode());
    }
}

impl CgroupDir {
    fn bind_control_files(self: &Arc<Self>) {
        for file in self.files.values() {
            file.bind_dir(self);
        }
    }
}
