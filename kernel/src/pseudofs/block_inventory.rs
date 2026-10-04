//! Linux-shaped registered block topology. Geometry is retained from GPT,
//! never inferred from a partition-looking name. No device configuration writes.
use alloc::{format, string::String, sync::Arc};
use core::fmt::Write;

use axfs_ng_vfs::{DeviceId, NodeType};

use super::{DirMapping, SimpleDir, SimpleFile, SimpleFs};
use crate::mounts;

pub(super) fn device_id(name: &str) -> Option<DeviceId> {
    if name == axfs::ROOT_BLOCK_DEVICE_NAME {
        return Some(mounts::ROOT_BLOCK_DEVICE_ID);
    }
    axfs::block_device_names()
        .iter()
        .position(|n| n == name)
        .and_then(mounts::extra_block_device_id)
}

fn relative_path(entry: &axfs::BlockInventoryEntry) -> String {
    match &entry.partition {
        Some(part) => format!("{}/{}", part.parent, entry.name),
        None => entry.name.clone(),
    }
}

pub(super) fn path(name: &str) -> String {
    axfs::block_inventory()
        .iter()
        .find(|e| e.name == name)
        .map(relative_path)
        .unwrap_or_else(|| name.into())
}

fn dev_text(id: DeviceId) -> String {
    format!("{}:{}\n", id.major(), id.minor())
}
fn uevent(name: &str, id: DeviceId, partition: Option<usize>) -> String {
    let mut out = format!(
        "MAJOR={}\nMINOR={}\nDEVNAME={name}\nDEVTYPE={}\n",
        id.major(),
        id.minor(),
        if partition.is_some() {
            "partition"
        } else {
            "disk"
        }
    );
    if let Some(number) = partition {
        let _ = writeln!(out, "PARTN={number}");
    }
    out
}

pub(super) fn augment_device(dir: &mut DirMapping, fs: &Arc<SimpleFs>, name: &str, id: DeviceId) {
    dir.add(
        "dev",
        SimpleFile::new_regular(fs.clone(), move || Ok(dev_text(id))),
    );
    dir.add(
        "removable",
        SimpleFile::new_regular(fs.clone(), || Ok("0\n")),
    );
    let owned = String::from(name);
    dir.add(
        "ro",
        SimpleFile::new_regular(fs.clone(), move || {
            axfs::block_device_is_read_only(&owned)
                .map(|ro| format!("{}\n", u8::from(ro)))
                .ok_or(axfs_ng_vfs::VfsError::NotFound)
        }),
    );
    let inventory = axfs::block_inventory();
    let partition = inventory
        .iter()
        .find(|entry| entry.name == name)
        .and_then(|entry| entry.partition.as_ref());
    if let Some(part) = partition {
        let number = part.number;
        let block_size = inventory
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
            .info
            .block_size;
        let start = part.start.saturating_mul(block_size as u64) / 512;
        dir.add(
            "partition",
            SimpleFile::new_regular(fs.clone(), move || Ok(format!("{number}\n"))),
        );
        dir.add(
            "start",
            SimpleFile::new_regular(fs.clone(), move || Ok(format!("{start}\n"))),
        );
    }
    let number = partition.map(|part| part.number);
    let owned = String::from(name);
    dir.add(
        "uevent",
        SimpleFile::new_regular(fs.clone(), move || Ok(uevent(&owned, id, number))),
    );
    // A partition is one directory deeper than its whole disk.
    let target = if number.is_some() {
        "../../../class/block"
    } else {
        "../../class/block"
    };
    dir.add(
        "subsystem",
        SimpleFile::new(fs.clone(), NodeType::Symlink, move || Ok(target)),
    );
    for entry in inventory
        .iter()
        .filter(|entry| entry.partition.as_ref().is_some_and(|p| p.parent == name))
    {
        if let Some(id) = device_id(&entry.name) {
            dir.add(
                &entry.name,
                super::sys::block_device_dir(fs.clone(), entry.name.clone(), id, entry.info),
            );
        }
    }
}

pub(super) fn augment_loop(dir: &mut DirMapping, fs: &Arc<SimpleFs>, id: DeviceId) {
    dir.add(
        "dev",
        SimpleFile::new_regular(fs.clone(), move || Ok(dev_text(id))),
    );
    dir.add(
        "removable",
        SimpleFile::new_regular(fs.clone(), || Ok("0\n")),
    );
    dir.add(
        "subsystem",
        SimpleFile::new(fs.clone(), NodeType::Symlink, || Ok("../../class/block")),
    );
}

pub(super) fn class_root(fs: Arc<SimpleFs>) -> DirMapping {
    let mut root = DirMapping::new();
    let mut block = DirMapping::new();
    for number in 0..16 {
        block.add(
            format!("loop{number}"),
            SimpleFile::new(fs.clone(), NodeType::Symlink, move || {
                Ok(format!("../../block/loop{number}"))
            }),
        );
    }
    for entry in axfs::block_inventory() {
        let target = format!("../../block/{}", relative_path(&entry));
        block.add(
            &entry.name,
            SimpleFile::new(fs.clone(), NodeType::Symlink, move || Ok(target.clone())),
        );
    }
    root.add("block", SimpleDir::new_maker(fs, Arc::new(block)));
    root
}

fn partition_row(out: &mut String, id: DeviceId, sectors: u64, name: &str) {
    if sectors != 0 {
        let _ = writeln!(
            out,
            "{:4}  {:7} {:10} {name}",
            id.major(),
            id.minor(),
            sectors / 2
        );
    }
}
fn partitions() -> String {
    let mut out = String::from("major minor  #blocks  name\n\n");
    for number in 0..16 {
        let sectors = super::dev::r#loop::snapshot(number).size_sectors;
        partition_row(
            &mut out,
            DeviceId::new(7, number),
            sectors,
            &format!("loop{number}"),
        );
    }
    for entry in axfs::block_inventory() {
        if let Some(id) = device_id(&entry.name) {
            partition_row(&mut out, id, entry.info.byte_len() / 512, &entry.name);
        }
    }
    out
}

pub(super) fn register_proc(root: &mut DirMapping, fs: &Arc<SimpleFs>) {
    root.add(
        "partitions",
        SimpleFile::new_regular(fs.clone(), || Ok(partitions())),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partition_row_uses_kib_and_omits_zero_capacity() {
        let mut out = String::new();
        partition_row(&mut out, DeviceId::new(259, 3), 2048, "nvme0n1p1");
        assert_eq!(out, " 259        3       1024 nvme0n1p1\n");
        partition_row(&mut out, DeviceId::new(7, 0), 0, "loop0");
        assert_eq!(out.lines().count(), 1);
    }
    #[test]
    fn validated_partition_metadata_controls_path_and_uevent() {
        let entry = axfs::BlockInventoryEntry {
            name: "nvme0n1p7".into(),
            info: axfs::BlockDeviceInfo {
                num_blocks: 10,
                block_size: 4096,
            },
            read_only: true,
            partition: Some(axdriver::PartitionMetadata {
                parent: "nvme0n1".into(),
                number: 7,
                start: 32,
            }),
        };
        assert_eq!(relative_path(&entry), "nvme0n1/nvme0n1p7");
        assert_eq!(
            uevent(&entry.name, DeviceId::new(259, 7), Some(7)),
            "MAJOR=259\nMINOR=7\nDEVNAME=nvme0n1p7\nDEVTYPE=partition\nPARTN=7\n"
        );
        assert_eq!(dev_text(DeviceId::new(259, 7)), "259:7\n");
    }
}
