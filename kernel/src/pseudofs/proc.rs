use alloc::{
    borrow::Cow,
    boxed::Box,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    any::Any,
    ffi::CStr,
    fmt::Write as _,
    iter, str,
    sync::atomic::{AtomicUsize, Ordering},
    task::Context,
};

use axfs::page_cache_pfn_is_dirty;
use axfs_ng_vfs::{
    FileNodeOps, Filesystem, FilesystemOps, Metadata, MetadataUpdate, NodeFlags, NodeOps,
    NodePermission, NodeType, VfsError, VfsResult,
};
use axhal::paging::MappingFlags;
use axpoll::{IoEvents, Pollable};
use axtask::{AxTaskRef, TaskState, WeakAxTaskRef, current};
use inherit_methods_macro::inherit_methods;
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};
use starry_process::Process;

use crate::{
    file::{FD_TABLE, FileDescription, inotify::InotifyFile, lease, pipe},
    mm::{Backend, BackendOps, system_memory_stats},
    mounts,
    pseudofs::{
        DirMaker, DirMapping, NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile,
        SimpleFileOperation, SimpleFs, SimpleFsNode, dev::RANDOM_ENTROPY_BITS,
    },
    syscall::{
        current_domainname_string, msg_next_id, msgmni_limit, proc_version_string,
        set_domainname_bytes, set_msg_next_id, set_msgmni_limit, set_shmmax_limit, shmall_limit,
        shmmax_limit, shmmni_limit, sysvipc_msg_snapshot,
    },
    task::{AsThread, get_task, get_visible_task_including_exiting, render_task_stat, tasks},
};

const PROC_PID_MAX: u32 = 4_194_304;
const PROC_THREADS_MAX: u32 = 4_194_304;
const PROC_PAGEMAP_ENTRY_BYTES: u64 = 8;
const PROC_KPAGEFLAGS_ENTRY_BYTES: u64 = 8;
const KPF_DIRTY: u64 = 1 << 4;
// Minimal gzip-compressed kernel config for LTP kconfig probes.
const PROC_CONFIG_GZ: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0x73, 0xf6, 0xf7, 0x73, 0xf3, 0x74,
    0x8f, 0x77, 0x0d, 0x73, 0xf5, 0x0b, 0x71, 0x73, 0xb1, 0xad, 0xe4, 0x72, 0x86, 0x08, 0xb8, 0xf8,
    0xf9, 0x87, 0x78, 0xba, 0x45, 0x22, 0x04, 0x7c, 0x1d, 0xfd, 0x5c, 0x1c, 0x43, 0xfc, 0x83, 0x22,
    0xe3, 0xdd, 0x3c, 0x7d, 0x5c, 0xe3, 0x7d, 0xfc, 0x9d, 0xbd, 0x3d, 0xfd, 0xdc, 0x6d, 0x2b, 0xb9,
    0x00, 0xe0, 0x5c, 0x5a, 0x48, 0x42, 0x00, 0x00, 0x00,
];

fn render_mounts() -> String {
    let mut out = String::from("proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n");
    for record in mounts::snapshot() {
        let _ = writeln!(
            out,
            "{} {} {} {} 0 0",
            record.source,
            record.target,
            record.fs_type,
            mounts::mount_options(record.flags)
        );
    }
    out
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
    format!(
        "MemTotal:       {total_kb:>8} kB\n\
         MemFree:        {free_kb:>8} kB\n\
         MemAvailable:   {available_kb:>8} kB\n\
         Buffers:               0 kB\n\
         Cached:         {cached_kb:>8} kB\n\
         SwapCached:            0 kB\n\
         Active:         {used_kb:>8} kB\n\
         Inactive:              0 kB\n\
         SwapTotal:             0 kB\n\
         SwapFree:              0 kB\n\
         Dirty:                 0 kB\n\
         Writeback:             0 kB\n\
         AnonPages:             0 kB\n\
         Mapped:         {mapped_kb:>8} kB\n\
         Shmem:                 0 kB\n\
         Slab:                  0 kB\n\
         PageTables:     {page_tables_kb:>8} kB\n\
         CommitLimit:    {total_kb:>8} kB\n\
         Committed_AS:   {used_kb:>8} kB\n\
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
        let task = get_visible_task_including_exiting(tid).map_err(|_| VfsError::NotFound)?;
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

#[rustfmt::skip]
fn task_status(task: &AxTaskRef) -> String {
    let locked_kb = {
        let aspace_handle = task.as_thread().proc_data.aspace();
        let aspace = aspace_handle.lock();
        aspace.locked_bytes() / 1024
    };
    format!(
        "Tgid:\t{}\n\
        Pid:\t{}\n\
        Uid:\t0 0 0 0\n\
        Gid:\t0 0 0 0\n\
        VmLck:\t{} kB\n\
        VmSwap:\t0 kB\n\
        NoNewPrivs:\t{}\n\
        Cpus_allowed:\t1\n\
        Cpus_allowed_list:\t0\n\
        Mems_allowed:\t1\n\
        Mems_allowed_list:\t0",
        task.as_thread().proc_data.proc.pid(),
        task.as_thread().tid(),
        locked_kb,
        task.as_thread().proc_data.no_new_privs() as u8
    )
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
        let mut out = format!(
            "pos:\t0\nflags:\t{:o}\nmnt_id:\t0\n",
            description.status_flags()
        );
        if let Ok(stat) = description.inner.stat() {
            let _ = writeln!(out, "ino:\t{}", stat.ino);
        }
        if let Some(inotify) = description.inner.downcast_ref::<InotifyFile>() {
            out.push_str(&inotify.fdinfo());
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

/// The /proc/[pid] directory
struct ThreadDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
    show_task_dir: bool,
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

impl SimpleDirOps for ThreadDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            [
                Some("stat"),
                Some("status"),
                Some("oom_score_adj"),
                self.show_task_dir.then_some("task"),
                Some("maps"),
                Some("smaps"),
                Some("pagemap"),
                Some("mounts"),
                Some("cmdline"),
                Some("coredump_filter"),
                Some("timerslack_ns"),
                Some("comm"),
                Some("exe"),
                Some("fd"),
                Some("fdinfo"),
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
            "pagemap" => ProcPagemapFile::new(fs, Arc::downgrade(&task)).into(),
            "mounts" => SimpleFile::new_regular(fs, move || Ok(render_mounts())).into(),
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
            "comm" => SimpleFile::new_regular(
                fs,
                RwFile::new(move |req| match req {
                    SimpleFileOperation::Read => {
                        let mut bytes = vec![0; 16];
                        let name = task.name();
                        let copy_len = name.len().min(15);
                        bytes[..copy_len].copy_from_slice(&name.as_bytes()[..copy_len]);
                        bytes[copy_len] = b'\n';
                        Ok(Some(bytes))
                    }
                    SimpleFileOperation::Write(data) => {
                        if !data.is_empty() {
                            let mut input = [0; 16];
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
        let task = if name == "self" {
            current().clone()
        } else {
            let tid = name.parse::<u32>().map_err(|_| VfsError::NotFound)?;
            get_visible_task_including_exiting(tid).map_err(|_| VfsError::NotFound)?
        };
        let node = NodeOpsMux::Dir(SimpleDir::new_maker(
            self.0.clone(),
            Arc::new(ThreadDir {
                fs: self.0.clone(),
                task: Arc::downgrade(&task),
                show_task_dir: true,
            }),
        ));
        Ok(node)
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

    let mut root = DirMapping::new();
    root.add(
        "mounts",
        SimpleFile::new_regular(fs.clone(), || Ok(render_mounts())),
    );
    root.add("sysvipc", {
        let mut sysvipc = DirMapping::new();
        sysvipc.add(
            "msg",
            SimpleFile::new_regular(fs.clone(), || Ok(sysvipc_msg_snapshot())),
        );
        SimpleDir::new_maker(fs.clone(), Arc::new(sysvipc))
    });
    root.add(
        "meminfo",
        SimpleFile::new_regular(fs.clone(), || Ok(real_meminfo())),
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
        "version",
        SimpleFile::new_regular(fs.clone(), || Ok(proc_version_string())),
    );
    root.add(
        "uptime",
        SimpleFile::new_regular(fs.clone(), || {
            let uptime = axhal::time::monotonic_time();
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
                            pipe::set_pipe_max_size(value);
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

            SimpleDir::new_maker(fs.clone(), Arc::new(fs_dir))
        });

        sys.add("kernel", {
            let mut kernel = DirMapping::new();

            kernel.add(
                "domainname",
                SimpleFile::new_regular(
                    fs.clone(),
                    RwFile::new(move |req| match req {
                        SimpleFileOperation::Read => Ok(Some(
                            format!("{}\n", current_domainname_string()).into_bytes(),
                        )),
                        SimpleFileOperation::Write(data) => {
                            if is_proc_truncate_write(data) {
                                return Ok(None);
                            }
                            let len = data
                                .iter()
                                .position(|&b| b == b'\n' || b == 0)
                                .unwrap_or(data.len());
                            set_domainname_bytes(&data[..len]);
                            Ok(None)
                        }
                    }),
                ),
            );
            kernel.add(
                "pid_max",
                SimpleFile::new_regular(fs.clone(), || Ok(alloc::format!("{PROC_PID_MAX}\n"))),
            );
            kernel.add(
                "threads-max",
                SimpleFile::new_regular(fs.clone(), || Ok(format!("{PROC_THREADS_MAX}\n"))),
            );
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
