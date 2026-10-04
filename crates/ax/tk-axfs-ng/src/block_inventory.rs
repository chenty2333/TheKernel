//! Read-only inventory of the registered root, whole disks and validated GPT views.
use alloc::{string::String, vec::Vec};
use core::sync::atomic::Ordering;

use crate::{BlockDeviceInfo, EXTRA_BLOCK_DEVICES, ROOT_BLOCK_DEVICE, RegisteredBlockDevice};

#[derive(Clone, Debug)]
pub struct BlockInventoryEntry {
    pub name: String,
    pub info: BlockDeviceInfo,
    pub read_only: bool,
    pub partition: Option<axdriver::PartitionMetadata>,
}

fn snapshot(entry: &RegisteredBlockDevice) -> BlockInventoryEntry {
    BlockInventoryEntry {
        name: entry.name.clone(),
        info: entry.info,
        read_only: entry.read_only.load(Ordering::Acquire),
        partition: entry.partition.clone(),
    }
}

pub fn block_inventory() -> Vec<BlockInventoryEntry> {
    let mut entries = Vec::new();
    if let Some(root) = ROOT_BLOCK_DEVICE.get() {
        entries.push(snapshot(root));
    }
    if let Some(extras) = EXTRA_BLOCK_DEVICES.get() {
        entries.extend(extras.lock().iter().map(snapshot));
    }
    entries
}
