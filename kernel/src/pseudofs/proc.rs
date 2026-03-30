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
    ffi::CStr,
    fmt::Write as _,
    iter,
    sync::atomic::{AtomicUsize, Ordering},
};

use axfs_ng_vfs::{Filesystem, NodeType, VfsError, VfsResult};
use axhal::paging::MappingFlags;
use axtask::{AxTaskRef, TaskState, WeakAxTaskRef, current};
use starry_process::Process;

use crate::{
    file::FD_TABLE,
    mm::{Backend, system_memory_stats},
    pseudofs::{
        DirMaker, DirMapping, NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile,
        SimpleFileOperation, SimpleFs,
    },
    task::{AsThread, TaskStat, get_task, get_visible_task, tasks},
};

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
        let task = get_visible_task(tid).map_err(|_| VfsError::NotFound)?;
        if task.as_thread().proc_data.proc.pid() != process.pid() {
            return Err(VfsError::NotFound);
        }

        Ok(NodeOpsMux::Dir(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(ThreadDir {
                fs: self.fs.clone(),
                task: Arc::downgrade(&task),
            }),
        )))
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

#[rustfmt::skip]
fn task_status(task: &AxTaskRef) -> String {
    format!(
        "Tgid:\t{}\n\
        Pid:\t{}\n\
        Uid:\t0 0 0 0\n\
        Gid:\t0 0 0 0\n\
        Cpus_allowed:\t1\n\
        Cpus_allowed_list:\t0\n\
        Mems_allowed:\t1\n\
        Mems_allowed_list:\t0",
        task.as_thread().proc_data.proc.pid(),
        task.as_thread().tid()
    )
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

/// The /proc/[pid] directory
struct ThreadDir {
    fs: Arc<SimpleFs>,
    task: WeakAxTaskRef,
}

impl SimpleDirOps for ThreadDir {
    fn child_names<'a>(&'a self) -> Box<dyn Iterator<Item = Cow<'a, str>> + 'a> {
        Box::new(
            [
                "stat",
                "status",
                "oom_score_adj",
                "task",
                "maps",
                "mounts",
                "cmdline",
                "comm",
                "exe",
                "fd",
            ]
            .into_iter()
            .map(Cow::Borrowed),
        )
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let fs = self.fs.clone();
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        Ok(match name {
            "stat" => SimpleFile::new_regular(fs, move || {
                Ok(format!("{}", TaskStat::from_thread(&task)?).into_bytes())
            })
            .into(),
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
            "task" => SimpleDir::new_maker(
                fs.clone(),
                Arc::new(ProcessTaskDir {
                    fs,
                    process: Arc::downgrade(&task.as_thread().proc_data.proc),
                }),
            )
            .into(),
            "maps" => SimpleFile::new_regular(fs, move || {
                let thr = task.as_thread();
                let aspace = thr.proc_data.aspace.lock();
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
                }
                Ok(out)
            })
            .into(),
            "mounts" => SimpleFile::new_regular(fs, move || {
                Ok("proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n")
            })
            .into(),
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
            get_visible_task(tid).map_err(|_| VfsError::NotFound)?
        };
        let node = NodeOpsMux::Dir(SimpleDir::new_maker(
            self.0.clone(),
            Arc::new(ThreadDir {
                fs: self.0.clone(),
                task: Arc::downgrade(&task),
            }),
        ));
        Ok(node)
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DirMapping::new();
    root.add(
        "mounts",
        SimpleFile::new_regular(fs.clone(), || {
            Ok("proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n")
        }),
    );
    root.add(
        "meminfo",
        SimpleFile::new_regular(fs.clone(), || Ok(real_meminfo())),
    );
    root.add(
        "meminfo2",
        SimpleFile::new_regular(fs.clone(), || {
            let allocator = axalloc::global_allocator();
            Ok(format!("{:?}\n", allocator.usages()))
        }),
    );
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

    root.add("sys", {
        let mut sys = DirMapping::new();

        sys.add("kernel", {
            let mut kernel = DirMapping::new();

            kernel.add(
                "pid_max",
                SimpleFile::new_regular(fs.clone(), || Ok("32768\n")),
            );

            SimpleDir::new_maker(fs.clone(), Arc::new(kernel))
        });

        SimpleDir::new_maker(fs.clone(), Arc::new(sys))
    });

    let proc_dir = ProcFsHandler(fs.clone());
    SimpleDir::new_maker(fs, Arc::new(proc_dir.chain(root)))
}
