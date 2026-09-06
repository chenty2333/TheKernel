//! Linux tracefs surface used by perf trace and dynamic probe control.

mod io_uring;
use alloc::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    format,
    string::String,
    sync::Arc,
};

use axfs_ng_vfs::{Filesystem, FsName, FsNameBuf, NodePermission, VfsError, VfsResult};
use axsync::spin::SpinNoIrq;
pub(crate) use io_uring::record as record_io_uring;

use crate::{
    perf_sources::{
        SCHED_SWITCH_TRACEPOINT_ID, register_tracefs_event, set_tracefs_event_enabled,
        tracefs_format, tracefs_id, unregister_tracefs_event,
    },
    pseudofs::{
        ChildNames, DirMapping, NodeOpsMux, RwFile, SimpleDir, SimpleDirOps, SimpleFile,
        SimpleFileOperation, SimpleFileOps, SimpleFs, try_boxed_names,
    },
    task::AsThread,
};

static TRACE_ENABLED: SpinNoIrq<bool> = SpinNoIrq::new(true);
static TRACE_FILTER: SpinNoIrq<String> = SpinNoIrq::new(String::new());

#[derive(Clone, Copy)]
enum DynamicProbe {
    Kprobe {
        address: u64,
        retprobe: bool,
    },
    Uprobe {
        file: crate::uprobe::UprobeFileKey,
        offset: u64,
        retprobe: bool,
    },
}

struct DynamicEvent {
    id: u64,
    probe: DynamicProbe,
    enabled: Arc<SpinNoIrq<bool>>,
    filter: Arc<SpinNoIrq<String>>,
}

impl Clone for DynamicEvent {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            probe: self.probe,
            enabled: self.enabled.clone(),
            filter: self.filter.clone(),
        }
    }
}

static DYNAMIC_EVENTS: SpinNoIrq<BTreeMap<String, DynamicEvent>> = SpinNoIrq::new(BTreeMap::new());

/// Dynamic probe names originate in the textual tracefs control protocol, but
/// directory lookup itself is byte-oriented.  Keep that protocol-specific
/// text at its boundary rather than decoding a VFS dentry name.
fn dynamic_name_parts(name: &str) -> Option<(&[u8], &[u8])> {
    let bytes = name.as_bytes();
    let separator = bytes.iter().position(|byte| *byte == b'/')?;
    Some((&bytes[..separator], &bytes[separator + 1..]))
}

pub(crate) fn new_tracefs() -> Filesystem {
    SimpleFs::new_with("tracefs".into(), 0x7472_6163, builder)
}

fn builder(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut root = DirMapping::new();
    root.add(
        "available_events",
        SimpleFile::new_regular(fs.clone(), || -> VfsResult<String> {
            let mut events = String::from(
                "sched:sched_switch\nsched:sched_wakeup\nraw_syscalls:sys_enter\nraw_syscalls:\
                 sys_exit\n",
            );
            for name in DYNAMIC_EVENTS.lock().keys() {
                events.push_str(name);
                events.push('\n');
            }
            Ok(events)
        }),
    );
    root.add("io_uring", io_directory(fs.clone()));
    root.add("kprobe_events", probe_control(fs.clone(), false));
    root.add("uprobe_events", probe_control(fs.clone(), true));
    root.add("events", events_dir(fs.clone()));
    SimpleDir::new_maker(fs, Arc::new(root))
}

// Kernel-private, global diagnostic capture. Do not impersonate a Linux
// trace event or perf_event_open source. Recheck credentials on every operation,
// including inherited descriptors opened before a privilege drop.
struct IoDiagnosticFile(&'static str);
impl SimpleFileOps for IoDiagnosticFile {
    fn default_permission(&self) -> NodePermission {
        NodePermission::from_bits_truncate(0o600)
    }
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        require_io_admin()?;
        let value = match self.0 {
            "trace" => io_uring::snapshot()?,
            "enable" => format!("{}\n", u8::from(io_uring::enabled())),
            "dropped" => format!("{}\n", io_uring::dropped()),
            _ => return Err(VfsError::NotFound),
        };
        Ok(Cow::Owned(value.into_bytes()))
    }
    fn write_all(&self, data: &[u8]) -> VfsResult<()> {
        require_io_admin()?;
        match self.0 {
            "trace" => io_uring::clear(),
            "enable" => {
                let value = match core::str::from_utf8(data)
                    .map_err(|_| VfsError::InvalidInput)?
                    .trim()
                {
                    "0" => false,
                    "1" => true,
                    _ => return Err(VfsError::InvalidInput),
                };
                io_uring::set_enabled(value);
            }
            _ => return Err(VfsError::PermissionDenied),
        }
        Ok(())
    }
}
fn require_io_admin() -> VfsResult<()> {
    // SimpleFile does not install an opener security credential. Authorize
    // the live caller, so inherited descriptors cannot retain root access.
    let credential = axtask::current().as_thread().current_cred();
    if credential.has_effective_capability(linux_raw_sys::general::CAP_SYS_ADMIN) {
        Ok(())
    } else {
        Err(VfsError::PermissionDenied)
    }
}
fn io_directory(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut map = DirMapping::new();
    for name in ["enable", "trace", "dropped"] {
        map.add(
            name,
            SimpleFile::new_regular(fs.clone(), IoDiagnosticFile(name)),
        );
    }
    SimpleDir::new_maker(fs, Arc::new(map))
}

fn event_leaf(
    fs: Arc<SimpleFs>,
    system: &'static str,
    name: &'static str,
) -> crate::pseudofs::DirMaker {
    Arc::new(move |this| {
        let mut map = DirMapping::new();
        let id = tracefs_id(system, name).unwrap_or(0);
        map.add(
            "id",
            SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                Ok(format!("{id}\n"))
            }),
        );
        let format = tracefs_format(system, name).unwrap_or("name: unknown\n");
        map.add(
            "format",
            SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                Ok(format.into())
            }),
        );
        map.add(
            "enable",
            SimpleFile::new_regular(
                fs.clone(),
                RwFile::new(|op| -> VfsResult<Option<String>> {
                    match op {
                        SimpleFileOperation::Read => Ok(Some(if *TRACE_ENABLED.lock() {
                            "1\n".into()
                        } else {
                            "0\n".into()
                        })),
                        SimpleFileOperation::Write(data) => {
                            *TRACE_ENABLED.lock() = !data.starts_with(b"0");
                            Ok(None)
                        }
                    }
                }),
            ),
        );
        map.add(
            "filter",
            SimpleFile::new_regular(
                fs.clone(),
                RwFile::new(|op| -> VfsResult<Option<String>> {
                    match op {
                        SimpleFileOperation::Read => {
                            Ok(Some(format!("{}\n", &*TRACE_FILTER.lock())))
                        }
                        SimpleFileOperation::Write(data) => {
                            let text =
                                core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                            *TRACE_FILTER.lock() = text.trim().into();
                            Ok(None)
                        }
                    }
                }),
            ),
        );
        (SimpleDir::new_maker(fs.clone(), Arc::new(map)))(this)
    })
}

/// `events` is deliberately non-cacheable. Dynamic probe removal must make a
/// following lookup fail rather than leave a positive dentry reachable until
/// a mount-wide cache invalidation happens.
struct TraceEventsDir {
    fs: Arc<SimpleFs>,
}

impl SimpleDirOps for TraceEventsDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let mut names = BTreeSet::new();
        names.insert(FsNameBuf::from_vec(b"sched".to_vec())?);
        names.insert(FsNameBuf::from_vec(b"raw_syscalls".to_vec())?);
        for key in DYNAMIC_EVENTS.lock().keys() {
            if let Some((system, _)) = dynamic_name_parts(key) {
                names.insert(FsNameBuf::from_vec(system.to_vec())?);
            }
        }
        try_boxed_names(names.into_iter().map(Cow::Owned))
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let static_system = matches!(name.as_bytes(), b"sched" | b"raw_syscalls");
        let dynamic_system = DYNAMIC_EVENTS
            .lock()
            .keys()
            .any(|key| dynamic_name_parts(key).is_some_and(|(group, _)| group == name.as_bytes()));
        if !static_system && !dynamic_system {
            return Err(VfsError::NotFound);
        }
        Ok(SimpleDir::new_maker(
            self.fs.clone(),
            Arc::new(TraceEventSystemDir {
                fs: self.fs.clone(),
                system: FsNameBuf::from_vec(name.as_bytes().to_vec())?,
            }),
        )
        .into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

struct TraceEventSystemDir {
    fs: Arc<SimpleFs>,
    system: FsNameBuf,
}

impl SimpleDirOps for TraceEventSystemDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let mut names = BTreeSet::new();
        match self.system.as_bytes() {
            b"sched" => {
                names.insert(FsNameBuf::from_vec(b"sched_switch".to_vec())?);
                names.insert(FsNameBuf::from_vec(b"sched_wakeup".to_vec())?);
            }
            b"raw_syscalls" => {
                names.insert(FsNameBuf::from_vec(b"sys_enter".to_vec())?);
                names.insert(FsNameBuf::from_vec(b"sys_exit".to_vec())?);
            }
            _ => {}
        }
        for key in DYNAMIC_EVENTS.lock().keys() {
            if let Some((group, event)) = dynamic_name_parts(key)
                && group == self.system.as_bytes()
            {
                names.insert(FsNameBuf::from_vec(event.to_vec())?);
            }
        }
        try_boxed_names(names.into_iter().map(Cow::Owned))
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        if matches!(
            (self.system.as_bytes(), name.as_bytes()),
            (b"sched", b"sched_switch")
                | (b"sched", b"sched_wakeup")
                | (b"raw_syscalls", b"sys_enter")
                | (b"raw_syscalls", b"sys_exit")
        ) {
            // Static names retain the original static leaf behavior.
            let system: &'static str = match self.system.as_bytes() {
                b"sched" => "sched",
                b"raw_syscalls" => "raw_syscalls",
                _ => return Err(VfsError::NotFound),
            };
            let name: &'static str = match name.as_bytes() {
                b"sched_switch" => "sched_switch",
                b"sched_wakeup" => "sched_wakeup",
                b"sys_enter" => "sys_enter",
                b"sys_exit" => "sys_exit",
                _ => return Err(VfsError::NotFound),
            };
            return Ok(event_leaf(self.fs.clone(), system, name).into());
        }
        let (full_name, dynamic) = DYNAMIC_EVENTS
            .lock()
            .iter()
            .find(|(full_name, _)| {
                dynamic_name_parts(full_name).is_some_and(|(system, event)| {
                    system == self.system.as_bytes() && event == name.as_bytes()
                })
            })
            .map(|(full_name, event)| (full_name.clone(), event.clone()))
            .ok_or(VfsError::NotFound)?;
        Ok(dynamic_event_leaf(self.fs.clone(), full_name, dynamic).into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

fn events_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    SimpleDir::new_maker(fs.clone(), Arc::new(TraceEventsDir { fs }))
}

fn dynamic_event_format(name: &str, event: DynamicEvent) -> String {
    let detail = match event.probe {
        DynamicProbe::Kprobe { .. } => "\tfield:unsigned long ip;\toffset:8;\tsize:8;\tsigned:0;\n",
        DynamicProbe::Uprobe { .. } => concat!(
            "\tfield:unsigned long ip;\toffset:8;\tsize:8;\tsigned:0;\n",
            "\tfield:u64 mount_id;\toffset:16;\tsize:8;\tsigned:0;\n",
            "\tfield:u64 device;\toffset:24;\tsize:8;\tsigned:0;\n",
            "\tfield:u64 inode;\toffset:32;\tsize:8;\tsigned:0;\n",
            "\tfield:u64 offset;\toffset:40;\tsize:8;\tsigned:0;\n",
        ),
    };
    format!(
        "name: {name}\nID: {}\nformat:\n\tfield:unsigned short \
         common_type;\toffset:0;\tsize:2;\tsigned:0;\n\tfield:unsigned char \
         common_flags;\toffset:2;\tsize:1;\tsigned:0;\n\tfield:unsigned char \
         common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n\tfield:int \
         common_pid;\toffset:4;\tsize:4;\tsigned:1;\n{detail}",
        event.id,
    )
}

fn dynamic_event_leaf(
    fs: Arc<SimpleFs>,
    name: String,
    event: DynamicEvent,
) -> crate::pseudofs::DirMaker {
    Arc::new(move |this| {
        let mut map = DirMapping::new();
        let id = event.id;
        map.add(
            "id",
            SimpleFile::new_regular(fs.clone(), move || Ok(format!("{id}\n"))),
        );
        let format = dynamic_event_format(&name, event.clone());
        map.add(
            "format",
            SimpleFile::new_regular(fs.clone(), move || Ok(format.clone())),
        );
        let enabled = event.enabled.clone();
        map.add(
            "enable",
            SimpleFile::new_regular(
                fs.clone(),
                RwFile::new(move |op| match op {
                    SimpleFileOperation::Read => Ok(Some::<String>(if *enabled.lock() {
                        "1\n".into()
                    } else {
                        "0\n".into()
                    })),
                    SimpleFileOperation::Write(data) => {
                        let value = !data.starts_with(b"0");
                        set_tracefs_event_enabled(id, value).map_err(|_| VfsError::NotFound)?;
                        *enabled.lock() = value;
                        Ok(None)
                    }
                }),
            ),
        );
        let filter = event.filter.clone();
        map.add(
            "filter",
            SimpleFile::new_regular(
                fs.clone(),
                RwFile::new(move |op| match op {
                    SimpleFileOperation::Read => Ok(Some(format!("{}\n", &*filter.lock()))),
                    SimpleFileOperation::Write(data) => {
                        let text =
                            core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
                        *filter.lock() = text.trim().into();
                        Ok(None)
                    }
                }),
            ),
        );
        (SimpleDir::new_maker(fs.clone(), Arc::new(map)))(this)
    })
}

fn probe_control(fs: Arc<SimpleFs>, uprobe: bool) -> Arc<SimpleFile> {
    SimpleFile::new_regular(
        fs,
        RwFile::new(move |op| -> VfsResult<Option<String>> {
            match op {
                SimpleFileOperation::Read => Ok(Some(String::new())),
                SimpleFileOperation::Write(data) => {
                    let text = core::str::from_utf8(data)
                        .map_err(|_| VfsError::InvalidInput)?
                        .trim();
                    if let Some(name) = text.strip_prefix("-:") {
                        let event = DYNAMIC_EVENTS
                            .lock()
                            .remove(name)
                            .ok_or(VfsError::NotFound)?;
                        unregister_tracefs_event(event.id).map_err(|_| VfsError::InvalidInput)?;
                        match event.probe {
                            DynamicProbe::Kprobe { address, retprobe } => {
                                crate::perf_sources::unregister_kprobe(address, retprobe)
                            }
                            DynamicProbe::Uprobe {
                                file,
                                offset,
                                retprobe,
                            } => crate::uprobe::unregister(file, offset, retprobe, 0),
                        }
                        return Ok(None);
                    }
                    let (kind, remainder) = text.split_once(':').ok_or(VfsError::InvalidInput)?;
                    if !matches!(kind, "p" | "r") {
                        return Err(VfsError::InvalidInput);
                    }
                    let (name, target) = remainder
                        .split_once(char::is_whitespace)
                        .ok_or(VfsError::InvalidInput)?;
                    let Some((group, event_name)) = name.split_once('/') else {
                        return Err(VfsError::InvalidInput);
                    };
                    if group.is_empty()
                        || event_name.is_empty()
                        || event_name.contains('/')
                        || DYNAMIC_EVENTS.lock().contains_key(name)
                    {
                        return Err(VfsError::InvalidInput);
                    }
                    let target = target.trim();
                    let probe = if uprobe {
                        let (path, offset) =
                            target.rsplit_once(':').ok_or(VfsError::InvalidInput)?;
                        let offset = parse_numeric_address(offset)?;
                        let file = crate::perf_sources::resolve_uprobe_path(path)
                            .map_err(|_| VfsError::InvalidInput)?;
                        let retprobe = kind == "r";
                        crate::uprobe::register(file, offset, retprobe, 0)
                            .map_err(|_| VfsError::NoMemory)?;
                        DynamicProbe::Uprobe {
                            file,
                            offset,
                            retprobe,
                        }
                    } else {
                        let address = parse_kprobe_target(target)?;
                        let retprobe = kind == "r";
                        crate::perf_sources::register_kprobe(address, retprobe)
                            .map_err(|_| VfsError::InvalidInput)?;
                        DynamicProbe::Kprobe { address, retprobe }
                    };
                    let mut key = 0xcbf2_9ce4_8422_2325u64;
                    for byte in text.bytes() {
                        key = (key ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3);
                    }
                    let source = match probe {
                        DynamicProbe::Kprobe { address, retprobe } => {
                            crate::file::PerfEvent::Kprobe {
                                addr: address,
                                retprobe,
                                query_offset: 0,
                            }
                        }
                        DynamicProbe::Uprobe {
                            file,
                            offset,
                            retprobe,
                        } => crate::file::PerfEvent::Uprobe {
                            mount_id: file.mount_id,
                            device: file.device,
                            inode: file.inode,
                            offset,
                            retprobe,
                            reference_counter_offset: 0,
                        },
                    };
                    let id = match register_tracefs_event(key.max(1), source) {
                        Ok(id) => id,
                        Err(error) => {
                            match probe {
                                DynamicProbe::Kprobe { address, retprobe } => {
                                    crate::perf_sources::unregister_kprobe(address, retprobe)
                                }
                                DynamicProbe::Uprobe {
                                    file,
                                    offset,
                                    retprobe,
                                } => crate::uprobe::unregister(file, offset, retprobe, 0),
                            }
                            return Err(match error {
                                axerrno::AxError::StorageFull | axerrno::AxError::NoMemory => {
                                    VfsError::NoMemory
                                }
                                _ => VfsError::InvalidInput,
                            });
                        }
                    };
                    DYNAMIC_EVENTS.lock().insert(
                        name.into(),
                        DynamicEvent {
                            id,
                            probe,
                            enabled: Arc::new(SpinNoIrq::new(true)),
                            filter: Arc::new(SpinNoIrq::new(String::new())),
                        },
                    );
                    Ok(None)
                }
            }
        }),
    )
}

fn parse_numeric_address(value: &str) -> VfsResult<u64> {
    let value = value.trim();
    let hex = value.strip_prefix("0x").unwrap_or(value);
    u64::from_str_radix(hex, if value.starts_with("0x") { 16 } else { 10 })
        .map_err(|_| VfsError::InvalidInput)
}

/// tracefs kprobe targets are either an explicitly written `0x` text address
/// or an exported `symbol+offset`.  Unlike uprobe offsets, the result must be
/// an admitted kernel/module instruction before registration can patch it.
fn parse_kprobe_target(value: &str) -> VfsResult<u64> {
    let value = value.trim();
    let (base, addend) = value.split_once('+').unwrap_or((value, "0"));
    let addend = parse_numeric_address(addend)?;
    let address = if base.starts_with("0x") {
        parse_numeric_address(base)?
            .checked_add(addend)
            .ok_or(VfsError::InvalidInput)?
    } else {
        crate::syscall::resolve_kprobe_symbol(base, addend).map_err(|_| VfsError::InvalidInput)?
    };
    crate::syscall::validate_kprobe_address(address).map_err(|_| VfsError::InvalidInput)?;
    Ok(address)
}

const _: u64 = SCHED_SWITCH_TRACEPOINT_ID;
