use alloc::{format, string::String, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use axfs_ng_vfs::{DeviceId, Filesystem, NodeType};
use axhal::mem::MemRegionFlags;

use crate::pseudofs::{DirMapping, SimpleDir, SimpleFile, SimpleFs, dev::r#loop as loopdev};

const LOOP_MAJOR: u32 = 7;
const VIRTIO_BLOCK_MAJOR: u32 = 8;
const BLOCK_LOGICAL_BLOCK_SIZE: u32 = 512;
const BLOCK_DMA_ALIGNMENT: u32 = 0;
const NUMA_NODE_COUNT: u32 = 2;
const FAKE_WRITE_SECTORS_PER_READ: u64 = 131_072;

static FAKE_BLOCK_WRITE_SECTORS: AtomicU64 = AtomicU64::new(0);
static FAKE_BLOCK_IO_TICKS: AtomicU64 = AtomicU64::new(1);

pub fn new_sysfs() -> Filesystem {
    SimpleFs::new_with("sysfs".into(), 0x6265_6572, builder)
}

fn builder(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut root = DirMapping::new();

    root.add("class", graphics_class_dir(fs.clone()));
    root.add("block", block_dir(fs.clone()));
    root.add("dev", dev_dir(fs.clone()));
    root.add("devices", devices_dir(fs.clone()));

    SimpleDir::new_maker(fs, Arc::new(root))
}

fn graphics_class_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut class = DirMapping::new();
    let mut graphics = DirMapping::new();
    let mut fb0 = DirMapping::new();
    let mut device = DirMapping::new();

    device.add(
        "subsystem",
        SimpleFile::new(fs.clone(), NodeType::Symlink, || Ok("whatever")),
    );
    fb0.add("device", SimpleDir::new_maker(fs.clone(), Arc::new(device)));
    graphics.add("fb0", SimpleDir::new_maker(fs.clone(), Arc::new(fb0)));
    class.add(
        "graphics",
        SimpleDir::new_maker(fs.clone(), Arc::new(graphics)),
    );

    SimpleDir::new_maker(fs, Arc::new(class))
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

    for (index, name) in axfs::block_device_names().into_iter().enumerate() {
        block.add(
            name.clone(),
            block_device_dir(
                fs.clone(),
                name,
                DeviceId::new(VIRTIO_BLOCK_MAJOR, 16 + index as u32),
            ),
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
            uevent_file(fs.clone(), name, dev_id),
        );
    }

    for (index, name) in axfs::block_device_names().into_iter().enumerate() {
        let dev_id = DeviceId::new(VIRTIO_BLOCK_MAJOR, 16 + index as u32);
        block.add(
            format!("{}:{}", dev_id.major(), dev_id.minor()),
            uevent_file(fs.clone(), name, dev_id),
        );
    }

    dev.add("block", SimpleDir::new_maker(fs.clone(), Arc::new(block)));
    SimpleDir::new_maker(fs, Arc::new(dev))
}

fn devices_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut devices = DirMapping::new();
    let mut system = DirMapping::new();
    system.add("node", node_root_dir(fs.clone()));
    devices.add("system", SimpleDir::new_maker(fs.clone(), Arc::new(system)));
    SimpleDir::new_maker(fs, Arc::new(devices))
}

fn node_root_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut node_root = DirMapping::new();
    let possible = format!("0-{}\n", NUMA_NODE_COUNT - 1);

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
    let cpu_list = format!("0-{}\n", cpu_count - 1);
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
    dir.add("hugepages", hugepages_dir(fs.clone()));
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn hugepages_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut hugepages = DirMapping::new();
    hugepages.add("hugepages-2048kB", hugepages_size_dir(fs.clone()));
    SimpleDir::new_maker(fs, Arc::new(hugepages))
}

fn hugepages_size_dir(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    dir.add(
        "nr_hugepages",
        SimpleFile::new_regular(fs.clone(), || Ok("0\n")),
    );
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn node_meminfo(node: u32) -> String {
    let kb = axhal::mem::memory_regions()
        .filter(|region| region.flags.contains(MemRegionFlags::FREE))
        .map(|region| region.size / 1024)
        .sum::<usize>()
        .max(131_072);
    let per_node_kb = kb / NUMA_NODE_COUNT as usize;
    let used_kb = per_node_kb / 4;
    let free_kb = per_node_kb.saturating_sub(used_kb);

    format!(
        "Node {node} MemTotal:       {per_node_kb} kB\nNode {node} MemFree:        {free_kb} \
         kB\nNode {node} MemUsed:        {used_kb} kB\nNode {node} Active:         0 kB\nNode \
         {node} Inactive:       0 kB\nNode {node} FilePages:      {free_kb} kB\n"
    )
}

fn block_device_dir(
    fs: Arc<SimpleFs>,
    dev_name: String,
    dev_id: DeviceId,
) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    let mut queue = DirMapping::new();
    queue.add(
        "logical_block_size",
        SimpleFile::new_regular(fs.clone(), || Ok(format!("{BLOCK_LOGICAL_BLOCK_SIZE}\n"))),
    );
    queue.add(
        "dma_alignment",
        SimpleFile::new_regular(fs.clone(), || Ok(format!("{BLOCK_DMA_ALIGNMENT}\n"))),
    );
    dir.add("stat", block_stat_file(fs.clone()));
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
        SimpleFile::new_regular(fs.clone(), || Ok(format!("{BLOCK_LOGICAL_BLOCK_SIZE}\n"))),
    );
    queue.add(
        "dma_alignment",
        SimpleFile::new_regular(fs.clone(), || Ok(format!("{BLOCK_DMA_ALIGNMENT}\n"))),
    );

    loop_dir.add(
        "partscan",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", loopdev::snapshot(number).partscan as u32))
        }),
    );
    loop_dir.add(
        "autoclear",
        SimpleFile::new_regular(fs.clone(), move || {
            Ok(format!("{}\n", loopdev::snapshot(number).autoclear as u32))
        }),
    );
    loop_dir.add(
        "backing_file",
        SimpleFile::new_regular(fs.clone(), move || {
            let backing_file = loopdev::snapshot(number).backing_file;
            Ok(if backing_file.is_empty() {
                "\n".into()
            } else {
                format!("{backing_file}\n")
            })
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

    dir.add("stat", block_stat_file(fs.clone()));
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
    dir.add(
        format!("loop{number}p1"),
        loop_partition_dir(fs.clone(), number),
    );
    dir.add("uevent", uevent_file(fs.clone(), dev_name, dev_id));
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn loop_partition_dir(fs: Arc<SimpleFs>, number: u32) -> crate::pseudofs::DirMaker {
    let mut dir = DirMapping::new();
    dir.add(
        "size",
        SimpleFile::new_regular(fs.clone(), move || {
            let snapshot = loopdev::snapshot(number);
            let sectors = if snapshot.partscan {
                snapshot.size_sectors
            } else {
                0
            };
            Ok(format!("{sectors}\n"))
        }),
    );
    SimpleDir::new_maker(fs, Arc::new(dir))
}

fn block_stat_file(fs: Arc<SimpleFs>) -> Arc<SimpleFile> {
    SimpleFile::new_regular(fs, || {
        let sectors = FAKE_BLOCK_WRITE_SECTORS
            .fetch_add(FAKE_WRITE_SECTORS_PER_READ, Ordering::Relaxed)
            + FAKE_WRITE_SECTORS_PER_READ;
        let ticks = FAKE_BLOCK_IO_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(format!("0 0 0 0 0 0 {sectors} 1 0 {ticks} {ticks}\n"))
    })
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
