//! Read-only inventory of the registered root, whole disks and validated GPT views.
use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use axdriver::prelude::{BaseDriverOps, BlockDriverOps, DevError};

use crate::{
    BlockDeviceInfo, EXTRA_BLOCK_DEVICES, ROOT_BLOCK_DEVICE, RegisteredBlockDevice,
    SharedBlockDevice,
};

#[derive(Clone, Debug)]
pub struct BlockInventoryEntry {
    pub name: String,
    pub info: BlockDeviceInfo,
    pub read_only: bool,
    pub partition: Option<axdriver::PartitionMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PartitionRescanError {
    NotFound,
    Busy,
    Invalid,
    Io,
    NoMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockDeviceChangeAction {
    Added,
    Removed,
}

/// Bounded callback invoked after the block topology lock is released.
pub type BlockDeviceChangeHook = fn(BlockDeviceChangeAction, BlockInventoryEntry);
static BLOCK_DEVICE_CHANGE_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Installs the single kernel uevent bridge after its dynamic device registry
/// is ready. Reinstalling the same callback is idempotent.
pub fn install_block_device_change_hook(hook: BlockDeviceChangeHook) -> bool {
    let address = hook as usize;
    match BLOCK_DEVICE_CHANGE_HOOK.compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => true,
        Err(existing) => existing == address,
    }
}

fn notify_change(action: BlockDeviceChangeAction, entry: BlockInventoryEntry) {
    let address = BLOCK_DEVICE_CHANGE_HOOK.load(Ordering::Acquire);
    if address != 0 {
        // SAFETY: the hook is a `fn` pointer installed once with this signature.
        let hook = unsafe { core::mem::transmute::<usize, BlockDeviceChangeHook>(address) };
        hook(action, entry);
    }
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

/// Withdraws whole disks whose driver has an authoritative absent-media
/// snapshot. The device set is sampled without holding the topology lock while
/// touching hardware; mounted queue claims make `remove_block_device` return
/// Busy and are retried on the next poll.
pub fn remove_absent_media_devices() -> usize {
    let candidates = EXTRA_BLOCK_DEVICES
        .get()
        .map(|devices| {
            devices
                .lock()
                .iter()
                .filter(|entry| entry.partition.is_none())
                .map(|entry| (entry.name.clone(), entry.device.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut removed = 0;
    for (name, device) in candidates {
        if device.media_presence() == Some(false) {
            match remove_block_device(&name) {
                Ok(()) => removed += 1,
                Err(PartitionRescanError::Busy) => {}
                Err(error) => log::debug!("block hot-remove {name} deferred: {error:?}"),
            }
        }
    }
    removed
}

/// Re-read one registered disk's GPT and atomically replace its partition
/// views. The parent disk remains registered on every parse/allocation error.
/// A mounted partition prevents replacement because its filesystem retains
/// the old view.
pub fn rescan_gpt_partitions(name: &str) -> Result<usize, PartitionRescanError> {
    if name == crate::ROOT_BLOCK_DEVICE_NAME {
        return Err(PartitionRescanError::Busy);
    }
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(PartitionRescanError::NotFound)?;
    let (parent, read_only, removable) = {
        let devices = devices.lock();
        let entry = devices
            .iter()
            .find(|entry| entry.name == name && entry.partition.is_none())
            .ok_or(PartitionRescanError::NotFound)?;
        (
            entry.device.clone(),
            entry.read_only.load(Ordering::Acquire),
            entry.info.removable,
        )
    };
    let partitions = axdriver::discover_gpt_partitions(&parent, name, read_only).map_err(
        |error| match error {
            DevError::InvalidParam => PartitionRescanError::Invalid,
            DevError::NoMemory => PartitionRescanError::NoMemory,
            _ => PartitionRescanError::Io,
        },
    )?;
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(partitions.len())
        .map_err(|_| PartitionRescanError::NoMemory)?;
    for partition in partitions {
        let metadata = axdriver::block_device_partition(&partition);
        let partition_name = String::from(partition.device_name());
        let device = SharedBlockDevice::new(partition);
        replacement.push(RegisteredBlockDevice {
            partition: metadata,
            name: partition_name,
            info: BlockDeviceInfo {
                removable,
                num_blocks: device.num_blocks(),
                block_size: device.block_size(),
            },
            read_only: AtomicBool::new(read_only),
            mounted: Arc::new(AtomicBool::new(false)),
            device,
        });
    }
    let mut devices = devices.lock();
    if devices
        .iter()
        .any(|entry| entry.name == name && entry.mounted.load(Ordering::Acquire))
    {
        return Err(PartitionRescanError::Busy);
    }
    if devices.iter().any(|entry| {
        entry
            .partition
            .as_ref()
            .is_some_and(|part| part.parent == name)
            && entry.mounted.load(Ordering::Acquire)
    }) {
        return Err(PartitionRescanError::Busy);
    }
    for candidate in &replacement {
        if devices.iter().any(|entry| {
            entry.name == candidate.name
                && !entry
                    .partition
                    .as_ref()
                    .is_some_and(|part| part.parent == name)
        }) {
            return Err(PartitionRescanError::Busy);
        }
    }
    let mut removed = Vec::new();
    let old_partitions = devices
        .iter()
        .filter(|entry| {
            entry
                .partition
                .as_ref()
                .is_some_and(|part| part.parent == name)
        })
        .count();
    removed
        .try_reserve_exact(old_partitions)
        .map_err(|_| PartitionRescanError::NoMemory)?;
    removed.extend(
        devices
            .iter()
            .filter(|entry| {
                entry
                    .partition
                    .as_ref()
                    .is_some_and(|part| part.parent == name)
            })
            .map(snapshot),
    );
    let mut added = Vec::new();
    added
        .try_reserve_exact(replacement.len())
        .map_err(|_| PartitionRescanError::NoMemory)?;
    added.extend(replacement.iter().map(snapshot));
    devices
        .try_reserve(replacement.len())
        .map_err(|_| PartitionRescanError::NoMemory)?;
    devices.retain(|entry| {
        !entry
            .partition
            .as_ref()
            .is_some_and(|part| part.parent == name)
    });
    let count = replacement.len();
    devices.extend(replacement);
    drop(devices);
    for entry in removed {
        notify_change(BlockDeviceChangeAction::Removed, entry);
    }
    for entry in added {
        notify_change(BlockDeviceChangeAction::Added, entry);
    }
    Ok(count)
}

/// Publish one block disk discovered after filesystem initialization. The
/// initial disk and its accepted GPT children are installed atomically.
pub fn add_block_device(raw: axdriver::AxBlockDevice) -> Result<String, PartitionRescanError> {
    if axdriver::block_device_partition(&raw).is_some() {
        return Err(PartitionRescanError::Invalid);
    }
    let name = String::from(raw.device_name());
    let read_only = axdriver::block_device_is_read_only(&raw);
    let removable = axdriver::block_device_removable(&raw);
    let parent = SharedBlockDevice::new(raw);
    let partitions = match axdriver::discover_gpt_partitions(&parent, &name, read_only) {
        Ok(partitions) => partitions,
        Err(DevError::InvalidParam) => Vec::new(),
        Err(DevError::NoMemory) => return Err(PartitionRescanError::NoMemory),
        Err(_) => return Err(PartitionRescanError::Io),
    };
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(partitions.len() + 1)
        .map_err(|_| PartitionRescanError::NoMemory)?;
    replacement.push(RegisteredBlockDevice {
        partition: None,
        name: name.clone(),
        info: BlockDeviceInfo {
            removable,
            num_blocks: parent.num_blocks(),
            block_size: parent.block_size(),
        },
        read_only: AtomicBool::new(read_only),
        mounted: Arc::new(AtomicBool::new(false)),
        device: parent,
    });
    for partition in partitions {
        let metadata = axdriver::block_device_partition(&partition);
        let partition_name = String::from(partition.device_name());
        let device = SharedBlockDevice::new(partition);
        replacement.push(RegisteredBlockDevice {
            partition: metadata,
            name: partition_name,
            info: BlockDeviceInfo {
                removable,
                num_blocks: device.num_blocks(),
                block_size: device.block_size(),
            },
            read_only: AtomicBool::new(read_only),
            mounted: Arc::new(AtomicBool::new(false)),
            device,
        });
    }
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(PartitionRescanError::Busy)?;
    let mut devices = devices.lock();
    if replacement
        .iter()
        .any(|new| devices.iter().any(|current| current.name == new.name))
    {
        return Err(PartitionRescanError::Busy);
    }
    devices
        .try_reserve(replacement.len())
        .map_err(|_| PartitionRescanError::NoMemory)?;
    let mut added = Vec::new();
    added
        .try_reserve_exact(replacement.len())
        .map_err(|_| PartitionRescanError::NoMemory)?;
    added.extend(replacement.iter().map(snapshot));
    devices.extend(replacement);
    drop(devices);
    for entry in added {
        notify_change(BlockDeviceChangeAction::Added, entry);
    }
    Ok(name)
}

/// Withdraw a runtime disk and its partition views. Mounted filesystems keep
/// their queue alive and therefore make removal fail with `Busy`.
pub fn remove_block_device(name: &str) -> Result<(), PartitionRescanError> {
    if name == crate::ROOT_BLOCK_DEVICE_NAME {
        return Err(PartitionRescanError::Busy);
    }
    let devices = EXTRA_BLOCK_DEVICES
        .get()
        .ok_or(PartitionRescanError::NotFound)?;
    let mut devices = devices.lock();
    let (is_partition, parent) = devices
        .iter()
        .find(|entry| entry.name == name)
        .map(|target| {
            (
                target.partition.is_some(),
                target
                    .partition
                    .as_ref()
                    .map(|part| part.parent.clone())
                    .unwrap_or_else(|| String::from(name)),
            )
        })
        .ok_or(PartitionRescanError::NotFound)?;
    if devices.iter().any(|entry| {
        (entry.name == parent
            || entry
                .partition
                .as_ref()
                .is_some_and(|part| part.parent == parent))
            && entry.mounted.load(Ordering::Acquire)
    }) {
        return Err(PartitionRescanError::Busy);
    }
    let removal_count = if is_partition {
        1
    } else {
        devices
            .iter()
            .filter(|entry| {
                entry.name == name
                    || entry
                        .partition
                        .as_ref()
                        .is_some_and(|part| part.parent == name)
            })
            .count()
    };
    let mut removed = Vec::new();
    removed
        .try_reserve_exact(removal_count)
        .map_err(|_| PartitionRescanError::NoMemory)?;
    removed.extend(
        devices
            .iter()
            .filter(|entry| {
                if is_partition {
                    entry.name == name
                } else {
                    entry.name == name
                        || entry
                            .partition
                            .as_ref()
                            .is_some_and(|part| part.parent == name)
                }
            })
            .map(snapshot),
    );
    if is_partition {
        devices.retain(|entry| entry.name != name);
    } else {
        devices.retain(|entry| {
            entry.name != name
                && !entry
                    .partition
                    .as_ref()
                    .is_some_and(|part| part.parent == name)
        });
    }
    drop(devices);
    for entry in removed {
        notify_change(BlockDeviceChangeAction::Removed, entry);
    }
    Ok(())
}
