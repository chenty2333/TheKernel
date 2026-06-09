use alloc::{format, string::String, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use axfs_ng_vfs::{DeviceId, Filesystem, NodeType};

use crate::pseudofs::{DirMapping, SimpleDir, SimpleFile, SimpleFs};

const LOOP_MAJOR: u32 = 7;
const VIRTIO_BLOCK_MAJOR: u32 = 8;
const BLOCK_LOGICAL_BLOCK_SIZE: u32 = 512;
const BLOCK_DMA_ALIGNMENT: u32 = 0;
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
            block_device_dir(fs.clone(), name, DeviceId::new(LOOP_MAJOR, i)),
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
