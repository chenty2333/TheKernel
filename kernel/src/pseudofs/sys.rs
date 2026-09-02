use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
};

use axfs_ng_vfs::{DeviceId, Filesystem, NodeType, VfsResult};

use crate::{
    mounts,
    pmu_registry::{PMU_EVENTS, PmuEvents, registered_pmus},
    pseudofs::{
        DirMapping, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs, dev::r#loop as loopdev,
        device_registry,
    },
};

const LOOP_MAJOR: u32 = 7;
const BLOCK_DMA_ALIGNMENT: u32 = 0;
const NUMA_NODE_COUNT: u32 = 1;

pub fn new_sysfs() -> Filesystem {
    SimpleFs::new_with("sysfs".into(), 0x6265_6572, builder)
}

fn builder(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut root = DirMapping::new();
    let mut fs_dir = DirMapping::new();
    let mut fuse_dir = DirMapping::new();
    fuse_dir.add("connections", empty_dir(fs.clone()));
    fs_dir.add("fuse", SimpleDir::new_maker(fs.clone(), Arc::new(fuse_dir)));

    root.add("class", class_dir(fs.clone()));
    root.add("block", block_dir(fs.clone()));
    root.add("dev", dev_dir(fs.clone()));
    root.add("devices", devices_dir(fs.clone()));
    root.add("bus", bus_dir(fs.clone()));
    root.add("kernel", kernel_dir(fs.clone()));
    root.add("fs", SimpleDir::new_maker(fs.clone(), Arc::new(fs_dir)));

    SimpleDir::new_maker(fs, Arc::new(root))
}

fn empty_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
}

fn kernel_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut kernel = DirMapping::new();
    let mut debug = DirMapping::new();
    let mut dri = DirMapping::new();
    dri.add("0", empty_dir(fs.clone()));
    debug.add("dri", SimpleDir::new_maker(fs.clone(), Arc::new(dri)));
    debug.add("tracing", empty_dir(fs.clone()));
    kernel.add("debug", SimpleDir::new_maker(fs.clone(), Arc::new(debug)));
    kernel.add("tracing", empty_dir(fs.clone()));
    kernel.add(
        "kexec_loaded",
        SimpleFile::new_regular(fs.clone(), || -> VfsResult<String> {
            Ok(format!(
                "{}\n",
                u8::from(crate::syscall::normal_image_loaded())
            ))
        }),
    );
    kernel.add(
        "kexec_crash_loaded",
        SimpleFile::new_regular(fs.clone(), || -> VfsResult<String> {
            Ok(format!(
                "{}\n",
                u8::from(crate::syscall::crash_image_loaded())
            ))
        }),
    );
    SimpleDir::new_maker(fs, Arc::new(kernel))
}

fn bus_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut bus = DirMapping::new();
    let mut event_source = DirMapping::new();
    let mut devices = DirMapping::new();
    for pmu in registered_pmus() {
        let mut pmu_dir = DirMapping::new();
        let type_file = pmu.type_file();
        pmu_dir.add(
            "type",
            SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                Ok(type_file.clone())
            }),
        );
        let cpus = pmu.cpus.clone();
        pmu_dir.add(
            "cpus",
            SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                Ok(cpus.clone())
            }),
        );
        let identifier = format!("{}\n", &pmu.identifier);
        pmu_dir.add(
            "identifier",
            SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                Ok(identifier.clone())
            }),
        );
        let max_precise = format!("{}\n", pmu.max_precise);
        pmu_dir.add(
            "max_precise",
            SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                Ok(max_precise.clone())
            }),
        );
        let mut format_dir = DirMapping::new();
        for (name, value) in pmu.format.iter() {
            format_dir.add(
                name,
                SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                    Ok(String::from(*value))
                }),
            );
        }
        pmu_dir.add(
            "format",
            SimpleDir::new_maker(fs.clone(), Arc::new(format_dir)),
        );
        let mut events_dir = DirMapping::new();
        let events: &[(&str, &str)] = match &pmu.events {
            PmuEvents::Architectural => &PMU_EVENTS,
            PmuEvents::Fixed(events) => events,
            PmuEvents::None => &[],
        };
        for (name, value) in events {
            events_dir.add(
                name,
                SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                    Ok(String::from(*value))
                }),
            );
            if let Some((scale, unit)) = pmu.event_metadata_for(name) {
                let scale_name = format!("{name}.scale");
                events_dir.add(
                    &scale_name,
                    SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                        Ok(scale.clone())
                    }),
                );
                let unit_name = format!("{name}.unit");
                events_dir.add(
                    &unit_name,
                    SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                        Ok(unit.clone())
                    }),
                );
            }
        }
        pmu_dir.add(
            "events",
            SimpleDir::new_maker(fs.clone(), Arc::new(events_dir)),
        );
        let mut caps_dir = DirMapping::new();
        for (name, value) in pmu.caps.iter() {
            caps_dir.add(
                name,
                SimpleFile::new_regular(fs.clone(), move || -> VfsResult<String> {
                    Ok(String::from(*value))
                }),
            );
        }
        pmu_dir.add("caps", SimpleDir::new_maker(fs.clone(), Arc::new(caps_dir)));
        devices.add(
            pmu.kind.name(),
            SimpleDir::new_maker(fs.clone(), Arc::new(pmu_dir)),
        );
    }
    event_source.add(
        "devices",
        SimpleDir::new_maker(fs.clone(), Arc::new(devices)),
    );
    bus.add(
        "event_source",
        SimpleDir::new_maker(fs.clone(), Arc::new(event_source)),
    );
    SimpleDir::new_maker(
        fs.clone(),
        Arc::new(bus.chain(device_registry::bus_root(fs))),
    )
}

fn class_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    // Device-registry publication supplies the complete graphics class
    // object.  A static empty fb0 directory would shadow that object and
    // prevent udev from reading its dev/uevent attributes.
    SimpleDir::new_maker(fs.clone(), Arc::new(device_registry::class_root(fs)))
}

fn block_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut block = DirMapping::new();

    for i in 0..16 {
        let name = format!("loop{i}");
        block.add(
            name.clone(),
            loop_block_device_dir(fs.clone(), i, name, DeviceId::new(LOOP_MAJOR, i)),
        );
    }

    if let Some(info) = axfs::root_block_device_info() {
        let name = axfs::ROOT_BLOCK_DEVICE_NAME.to_string();
        block.add(
            name.clone(),
            block_device_dir(fs.clone(), name, mounts::ROOT_BLOCK_DEVICE_ID, info),
        );
    }

    for (index, name) in axfs::block_device_names().into_iter().enumerate() {
        let Some(info) = axfs::block_device_info(&name) else {
            continue;
        };
        let Some(dev_id) = mounts::extra_block_device_id(index) else {
            continue;
        };
        block.add(
            name.clone(),
            block_device_dir(fs.clone(), name, dev_id, info),
        );
    }

    SimpleDir::new_maker(fs, Arc::new(block))
}

fn dev_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut dev = DirMapping::new();
    let mut block = DirMapping::new();

    for i in 0..16 {
        let name = format!("loop{i}");
        let dev_id = DeviceId::new(LOOP_MAJOR, i);
        block.add(
            format!("{}:{}", dev_id.major(), dev_id.minor()),
            block_device_link(fs.clone(), name),
        );
    }

    if axfs::root_block_device_info().is_some() {
        let dev_id = mounts::ROOT_BLOCK_DEVICE_ID;
        block.add(
            format!("{}:{}", dev_id.major(), dev_id.minor()),
            block_device_link(fs.clone(), axfs::ROOT_BLOCK_DEVICE_NAME.to_string()),
        );
    }

    for (index, name) in axfs::block_device_names().into_iter().enumerate() {
        let Some(dev_id) = mounts::extra_block_device_id(index) else {
            continue;
        };
        block.add(
            format!("{}:{}", dev_id.major(), dev_id.minor()),
            block_device_link(fs.clone(), name),
        );
    }

    dev.add("block", SimpleDir::new_maker(fs.clone(), Arc::new(block)));
    dev.add(
        "char",
        SimpleDir::new_maker(
            fs.clone(),
            Arc::new(device_registry::dev_char_root(fs.clone())),
        ),
    );
    SimpleDir::new_maker(fs, Arc::new(dev))
}

fn block_device_link(fs: Arc<SimpleFs>, dev_name: String) -> Arc<SimpleFile> {
    SimpleFile::new(fs, NodeType::Symlink, move || {
        Ok(format!("../../block/{dev_name}"))
    })
}

fn devices_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut devices = DirMapping::new();
    let mut system = DirMapping::new();
    system.add("cpu", cpu_root_dir(fs.clone()));
    system.add("node", node_root_dir(fs.clone()));
    devices.add("system", SimpleDir::new_maker(fs.clone(), Arc::new(system)));
    SimpleDir::new_maker(
        fs.clone(),
        Arc::new(devices.chain(device_registry::devices_root(fs))),
    )
}

fn contiguous_index_list(count: usize) -> String {
    match count.max(1) {
        1 => "0\n".into(),
        count => format!("0-{}\n", count - 1),
    }
}

fn cpu_root_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut cpu_root = DirMapping::new();
    // TheKernel currently has no CPU hotplug: every configured CPU completes
    // secondary initialization before userspace starts.  Keep the three Linux
    // topology views identical until a real online/offline state machine is
    // introduced, so libc can discover the runtime topology without guessing
    // from unrelated procfs files.
    let online = contiguous_index_list(axhal::cpu_num());
    for name in ["online", "possible", "present"] {
        cpu_root.add(
            name,
            SimpleFile::new_regular(fs.clone(), {
                let online = online.clone();
                move || Ok(online.clone())
            }),
        );
    }

    SimpleDir::new_maker(fs, Arc::new(cpu_root))
}

fn node_root_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut node_root = DirMapping::new();
    let possible = if NUMA_NODE_COUNT == 1 {
        "0\n".into()
    } else {
        format!("0-{}\n", NUMA_NODE_COUNT - 1)
    };

    node_root.add(
        "possible",
        SimpleFile::new_regular(fs.clone(), {
            let possible = possible.clone();
            move || Ok(possible.clone())
        }),
    );
    node_root.add(
        "online",
        SimpleFile::new_regular(fs.clone(), {
            let possible = possible.clone();
            move || Ok(possible.clone())
        }),
    );
    node_root.add(
        "has_normal_memory",
        SimpleFile::new_regular(fs.clone(), {
            let possible = possible.clone();
            move || Ok(possible.clone())
        }),
    );
    node_root.add(
        "has_cpu",
        SimpleFile::new_regular(fs.clone(), {
            let possible = possible.clone();
            move || Ok(possible.clone())
        }),
    );

    for node in 0..NUMA_NODE_COUNT {
        node_root.add(format!("node{node}"), node_dir(fs.clone(), node));
    }

    SimpleDir::new_maker(fs, Arc::new(node_root))
}

fn node_dir(fs: Arc<SimpleFs>, node: u32) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    let cpu_count = axhal::cpu_num().max(1);
    let cpu_list = contiguous_index_list(cpu_count);
    let cpumap = if cpu_count >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << cpu_count) - 1
    };

    dir.add(
        "cpulist",
        SimpleFile::new_regular(fs.clone(), move || Ok(cpu_list.clone())),
    );
    dir.add(
        "cpumap",
        SimpleFile::new_regular(fs.clone(), move || Ok(format!("{cpumap:x}\n"))),
    );
    dir.add(
        "meminfo",
        SimpleFile::new_regular(fs.clone(), move || Ok(node_meminfo(node))),
    );
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn node_meminfo(node: u32) -> String {
    let stats = crate::mm::system_memory_stats();
    let total_kb = stats.total_bytes / 1024;
    let free_kb = stats.free_bytes / 1024;
    let used_kb = stats.used_bytes / 1024;
    let cached_kb = stats.cached_bytes / 1024;

    format!(
        "Node {node} MemTotal:       {total_kb} kB\nNode {node} MemFree:        {free_kb} \
         kB\nNode {node} MemUsed:        {used_kb} kB\nNode {node} FilePages:      {cached_kb} \
         kB\n"
    )
}

fn block_device_dir(
    fs: Arc<SimpleFs>,
    dev_name: String,
    dev_id: DeviceId,
    info: axfs::BlockDeviceInfo,
) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    let mut queue = DirMapping::new();
    queue.add(
        "logical_block_size",
        SimpleFile::new_regular(fs.clone(), move || Ok(format!("{}\n", info.block_size))),
    );
    dir.add(
        "size",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", info.byte_len() / 512))
        }),
    );
    queue.add(
        "dma_alignment",
        SimpleFile::new_regular(fs.clone(), || Ok(format!("{BLOCK_DMA_ALIGNMENT}\n"))),
    );
    dir.add("queue", SimpleDir::new_maker(fs.clone(), Arc::new(queue)));
    dir.add("uevent", uevent_file(fs.clone(), dev_name, dev_id));
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn loop_block_device_dir(
    fs: Arc<SimpleFs>,
    number: u32,
    dev_name: String,
    dev_id: DeviceId,
) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    let mut queue = DirMapping::new();
    let mut loop_dir = DirMapping::new();

    queue.add(
        "logical_block_size",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", loopdev::snapshot(number).block_size))
        }),
    );
    queue.add(
        "dma_alignment",
        SimpleFile::new_regular(fs.clone(), || Ok(format!("{BLOCK_DMA_ALIGNMENT}\n"))),
    );

    loop_dir.add(
        "partscan",
        SimpleFile::new_regular(fs.clone(), || Ok("0\n")),
    );
    loop_dir.add(
        "autoclear",
        SimpleFile::new_regular(fs.clone(), || Ok("0\n")),
    );
    loop_dir.add(
        "backing_file",
        SimpleFile::new_regular(fs.clone(), move || {
            let backing_file = loopdev::snapshot(number).backing_file;
            let mut bytes = backing_file;
            bytes.push(b'\n');
            Ok(bytes)
        }),
    );
    loop_dir.add(
        "dio",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", loopdev::snapshot(number).direct_io as u32))
        }),
    );
    loop_dir.add(
        "sizelimit",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", loopdev::snapshot(number).sizelimit))
        }),
    );

    dir.add(
        "size",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", loopdev::snapshot(number).size_sectors))
        }),
    );
    dir.add(
        "ro",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", loopdev::snapshot(number).read_only as u32))
        }),
    );
    dir.add("queue", SimpleDir::new_maker(fs.clone(), Arc::new(queue)));
    dir.add("loop", SimpleDir::new_maker(fs.clone(), Arc::new(loop_dir)));
    dir.add("uevent", uevent_file(fs.clone(), dev_name, dev_id));
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn uevent_file(fs: Arc<SimpleFs>, dev_name: String, dev_id: DeviceId) -> Arc<SimpleFile> {
    SimpleFile::new_regular(fs, move || {
        Ok(format!(
            "MAJOR={}\nMINOR={}\nDEVNAME={dev_name}\nDEVTYPE=disk\n",
            dev_id.major(),
            dev_id.minor()
        ))
    })
}

#[cfg(test)]
mod tests {
    use axfs::FsContext;
    use axfs_ng_vfs::{FsPath, Mountpoint};

    use super::{contiguous_index_list, new_sysfs};

    #[test]
    fn cpu_topology_list_is_nonempty_and_tracks_all_configured_cpus() {
        assert_eq!(contiguous_index_list(0), "0\n");
        assert_eq!(contiguous_index_list(1), "0\n");
        assert_eq!(contiguous_index_list(4), "0-3\n");
        assert_eq!(contiguous_index_list(8), "0-7\n");
    }

    #[test]
    fn sysfs_declares_pseudofs_mountpoints() {
        let _context = crate::test_support::scheduler_test_context();
        let filesystem = new_sysfs();
        let root = Mountpoint::new_root(&filesystem);
        let context = FsContext::new(root.root_location());

        for path in [
            b"/fs/fuse/connections".as_slice(),
            b"/kernel/tracing",
            b"/kernel/debug/tracing",
            b"/kernel/debug/dri/0",
        ] {
            assert!(context.resolve(FsPath::new(path)).is_ok(), "{path:?}");
        }
    }
}
