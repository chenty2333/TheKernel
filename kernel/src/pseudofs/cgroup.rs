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
use slab::Slab;
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};

use super::pseudo_stat_fs;
use crate::{
    file::{Directory, OpenCredentials, current_file_write_credentials, get_typed_file},
    task::{AsThread, get_process_data, send_signal_to_process},
};

const CGROUP_SUPER_MAGIC: u32 = 0x27e0_eb;
const CGROUP2_SUPER_MAGIC: u32 = 0x6367_7270;

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

pub fn parse_v1_controllers(source: &str, data: &str) -> AxResult<Vec<String>> {
    let mut controllers = Vec::new();
    for token in source.split(',') {
        let token = token.trim();
        if ALL_CONTROLLERS.contains(&token) && !controllers.iter().any(|it| it == token) {
            controllers.push(token.to_string());
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
            controllers.push(token.to_string());
        } else if KNOWN_V1_CONTROLLERS.contains(&token) {
            return Err(AxError::NoSuchDevice);
        } else {
            return Err(AxError::InvalidInput);
        }
    }
    if controllers.is_empty() {
        controllers.push("pids".to_string());
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
}

impl CgroupDir {
    fn new_root(fs: Arc<CgroupFs>) -> Arc<Self> {
        Self::new(fs, None)
    }

    fn new(fs: Arc<CgroupFs>, parent: Option<Weak<CgroupDir>>) -> Arc<Self> {
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
        });
        dir.bind_control_files();
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
            current = dir.parent.as_ref().and_then(Weak::upgrade);
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
            current = dir.parent.as_ref().and_then(Weak::upgrade);
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
                "cgroup.kill" => self.parent.is_some(),
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
        let this = self.this_dir()?;
        detach_mapped_pid(pid);
        self.node.fs.remove_pid_everywhere(pid);
        self.pids.lock().insert(pid);
        PID_CGROUPS.lock().insert(pid, Arc::downgrade(&this));
        this.update_pids_peak_hierarchy();
        Ok(())
    }

    fn attach_fork_child(self: &Arc<Self>, pid: Pid) {
        self.pids.lock().insert(pid);
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
        let activate_pids = enable.contains("pids") && !next.contains("pids");
        let deactivate_pids = disable.contains("pids") && next.contains("pids");
        for name in enable {
            next.insert(name);
        }
        for name in disable {
            next.remove(&name);
        }
        *self.subtree_control.lock() = next;
        if activate_pids || deactivate_pids {
            let children = self.children.lock().values().cloned().collect::<Vec<_>>();
            for child in children {
                if activate_pids {
                    child.initialize_pids_controller();
                } else {
                    child.reset_pids_controller();
                }
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

pub(crate) fn detach_process(pid: Pid) {
    detach_mapped_pid(pid);
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
        entries.extend(self.files.iter().filter_map(|(name, file)| {
            self.control_file_visible(name).then(|| {
                (
                    Cow::Owned(name.clone()),
                    file.node.ino,
                    NodeType::RegularFile,
                )
            })
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
        if self.control_file_visible(name)
            && let Some(file) = self.files.get(name).cloned()
        {
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
    dir: Mutex<Option<Weak<CgroupDir>>>,
}

impl CgroupFile {
    fn new(fs: Arc<CgroupFs>, name: &'static str) -> Arc<Self> {
        let mode = NodePermission::from_bits_truncate(match name {
            "cgroup.kill" => 0o200,
            _ if is_read_only_control_file(name) => 0o444,
            _ => 0o644,
        });
        Arc::new(Self {
            node: CgroupNode::new(fs, NodeType::RegularFile, mode),
            name,
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
        if !dir.control_file_visible(self.name) {
            return Err(VfsError::NotFound);
        }
        Ok(match self.name {
            "tasks" | "cgroup.procs" => dir.tasks_text(),
            "cgroup.controllers" => dir.controllers_text(),
            "cgroup.subtree_control" => dir.subtree_control_text(),
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
                dir.kill_attached_recursive();
                Ok(())
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
}

impl CgroupDir {
    fn bind_control_files(self: &Arc<Self>) {
        for file in self.files.values() {
            file.bind_dir(self);
        }
    }
}
