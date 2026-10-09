//! Linux-shaped registered block topology. Geometry is retained from GPT,
//! never inferred from a partition-looking name. No device configuration writes.
use alloc::{borrow::Cow, format, string::String, sync::Arc, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use axfs_ng_vfs::{DeviceId, FsName, FsNameBuf, NodeType, VfsError, VfsResult};
use axsync::Mutex;
use lazy_static::lazy_static;

use super::{
    ChildNames, DirMapping, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFile, SimpleFs,
    device_registry::{
        DeviceHandle, DeviceIdentity, DeviceRegistration, MAX_DEVICES, global_device_registry,
    },
    try_boxed_names,
};
use crate::mounts;

lazy_static! {
    static ref BLOCK_DEVICE_HANDLES: Mutex<Vec<(String, DeviceHandle<'static, MAX_DEVICES>)>> =
        Mutex::new(Vec::new());
}

static BLOCK_MEDIA_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

/// Starts the bounded card/link-detect poller after AXFS and uevent dispatch
/// are available. Only drivers returning an explicit absent-media fact are
/// affected; mounted devices remain registered until their claims are released.
pub fn start_media_poll_worker() -> axerrno::AxResult<()> {
    if BLOCK_MEDIA_WORKER_STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    if let Err(error) = axtask::spawn_raw(
        || loop {
            let removed = axfs::remove_absent_media_devices();
            if removed != 0 {
                info!("block hotplug: withdrew {removed} absent disk(s)");
            }
            let _ = axtask::sleep(Duration::from_millis(500));
        },
        "block_media_poll".into(),
        axconfig::TASK_STACK_SIZE,
    ) {
        BLOCK_MEDIA_WORKER_STARTED.store(false, Ordering::Release);
        return Err(error);
    }
    Ok(())
}

/// Starts the poller once a registered block device reports media presence.
/// Boot with only fixed disks therefore spawns no polling task; the first
/// removable registration starts it lazily.
pub fn start_media_poll_worker_for(name: &str) -> axerrno::AxResult<()> {
    if !axfs::block_device_media_presence_capable(name) {
        return Ok(());
    }
    start_media_poll_worker()
}

fn publish_block_uevent_device(entry: &axfs::BlockInventoryEntry) -> VfsResult<()> {
    let mut handles = BLOCK_DEVICE_HANDLES.lock();
    if handles.iter().any(|(name, _)| name == &entry.name) {
        return Ok(());
    }
    handles.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    let id = device_id(&entry.name).ok_or(VfsError::InvalidInput)?;
    let mut identity = DeviceIdentity::new("block".into(), "block".into(), entry.name.clone(), id)?;
    if let Some(partition) = &entry.partition {
        identity = identity.child_of("block".into(), partition.parent.clone())?;
    }
    let registration = DeviceRegistration::try_new(
        identity.clone(),
        if entry.partition.is_some() {
            "partition"
        } else {
            "disk"
        }
        .into(),
        Vec::new(),
        None,
    )?;
    let handle = global_device_registry()
        .reserve(identity)?
        .publish(registration)?;
    handles.push((entry.name.clone(), handle));
    Ok(())
}

fn remove_block_uevent_device(name: &str) -> VfsResult<()> {
    let handle = {
        let mut handles = BLOCK_DEVICE_HANDLES.lock();
        let index = handles
            .iter()
            .position(|(entry_name, _)| entry_name == name)
            .ok_or(VfsError::NotFound)?;
        handles.remove(index).1
    };
    handle.remove()
}

fn block_device_change(action: axfs::BlockDeviceChangeAction, entry: axfs::BlockInventoryEntry) {
    let result = match action {
        axfs::BlockDeviceChangeAction::Added => publish_block_uevent_device(&entry),
        axfs::BlockDeviceChangeAction::Removed => remove_block_uevent_device(&entry.name),
    };
    if action == axfs::BlockDeviceChangeAction::Added
        && let Err(error) = start_media_poll_worker_for(&entry.name)
    {
        warn!("block hotplug: media poll worker unavailable: {error:?}");
    }
    if let Err(error) = result {
        warn!(
            "block uevent registry {} {} failed: {error:?}",
            if action == axfs::BlockDeviceChangeAction::Added {
                "add"
            } else {
                "remove"
            },
            entry.name
        );
    }
}

pub(super) fn install_uevent_bridge() {
    if !axfs::install_block_device_change_hook(block_device_change) {
        warn!("block uevent hook is already owned by another provider");
        return;
    }
    for entry in axfs::block_inventory() {
        block_device_change(axfs::BlockDeviceChangeAction::Added, entry);
    }
}

pub(crate) fn device_id(name: &str) -> Option<DeviceId> {
    if name == axfs::ROOT_BLOCK_DEVICE_NAME {
        return Some(mounts::ROOT_BLOCK_DEVICE_ID);
    }
    if let Some(minor) = sd_device_minor(name) {
        return Some(DeviceId::new(8, minor));
    }
    if let Some(minor) = mmc_device_minor(name) {
        return Some(DeviceId::new(179, minor));
    }
    if let Some(minor) = nvme_device_minor(name) {
        return Some(DeviceId::new(259, minor));
    }
    axfs::block_device_names()
        .iter()
        .position(|n| n == name)
        .and_then(mounts::extra_block_device_id)
}

fn disk_letters_index(letters: &str) -> Option<u32> {
    if letters.is_empty() || !letters.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return None;
    }
    letters
        .bytes()
        .try_fold(0u32, |value, byte| {
            value
                .checked_mul(26)?
                .checked_add(u32::from(byte - b'a' + 1))
        })
        .and_then(|value| value.checked_sub(1))
}

fn sd_device_minor(name: &str) -> Option<u32> {
    let tail = name.strip_prefix("sd")?;
    let digit = tail
        .find(|ch: char| ch.is_ascii_digit())
        .unwrap_or(tail.len());
    let disk = disk_letters_index(&tail[..digit])?;
    let partition = if digit == tail.len() {
        0
    } else {
        tail[digit..].parse::<u32>().ok()?
    };
    disk.checked_add(1)?.checked_mul(16)?.checked_add(partition)
}

fn mmc_device_minor(name: &str) -> Option<u32> {
    let tail = name.strip_prefix("mmcblk")?;
    let p = tail.find('p').unwrap_or(tail.len());
    let disk = tail[..p].parse::<u32>().ok()?;
    let partition = if p == tail.len() {
        0
    } else {
        tail[p + 1..].parse::<u32>().ok()?
    };
    disk.checked_mul(8)?.checked_add(partition)
}

fn nvme_device_minor(name: &str) -> Option<u32> {
    let tail = name.strip_prefix("nvme")?;
    let n = tail.find('n')?;
    let controller = tail[..n].parse::<u32>().ok()?;
    let rest = &tail[n + 1..];
    let p = rest.find('p').unwrap_or(rest.len());
    let namespace = rest[..p].parse::<u32>().ok()?.checked_sub(1)?;
    let partition = if p == rest.len() {
        0
    } else {
        rest[p + 1..].parse::<u32>().ok()?
    };
    controller
        .checked_mul(1 << 20)?
        .checked_add(namespace.checked_mul(16)?)?
        .checked_add(partition)
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

fn removable_text(removable: Option<bool>) -> axfs_ng_vfs::VfsResult<String> {
    removable
        .map(|value| format!("{}\n", u8::from(value)))
        .ok_or(axfs_ng_vfs::VfsError::OperationNotSupported)
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
    super::block_statistics::augment_device(dir, fs, name);
    dir.add(
        "dev",
        SimpleFile::new_regular(fs.clone(), move || Ok(dev_text(id))),
    );
    let removable = axfs::block_device_info(name).and_then(|info| info.removable);
    dir.add(
        "removable",
        SimpleFile::new_regular(fs.clone(), move || removable_text(removable)),
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
    root.add("block", block_class_root(fs));
    root
}

pub(super) fn block_class_root(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    SimpleDir::new_maker(fs.clone(), Arc::new(BlockClassOps { fs }))
}

pub(super) fn block_root(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    SimpleDir::new_maker(fs.clone(), Arc::new(BlockRootOps { fs }))
}

pub(super) fn dev_block_root(fs: Arc<SimpleFs>) -> crate::pseudofs::DirMaker {
    SimpleDir::new_maker(fs.clone(), Arc::new(DevBlockRootOps { fs }))
}

fn owned_names<'a>(names: impl IntoIterator<Item = String>) -> VfsResult<ChildNames<'a>> {
    let mut owned = Vec::new();
    for name in names {
        owned.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        owned.push(FsNameBuf::from_vec(name.into_bytes()).map_err(|_| VfsError::InvalidInput)?);
    }
    try_boxed_names(owned.into_iter().map(Cow::Owned))
}

struct BlockClassOps {
    fs: Arc<SimpleFs>,
}

impl SimpleDirOps for BlockClassOps {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let mut names = Vec::new();
        for number in 0..16 {
            names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            names.push(format!("loop{number}"));
        }
        for entry in axfs::block_inventory() {
            names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            names.push(entry.name);
        }
        owned_names(names)
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let name = core::str::from_utf8(name.as_bytes()).map_err(|_| VfsError::NotFound)?;
        let target = if let Some(number) = name
            .strip_prefix("loop")
            .and_then(|n| n.parse::<u32>().ok())
        {
            if number >= 16 {
                return Err(VfsError::NotFound);
            }
            format!("../../block/loop{number}")
        } else {
            let entry = axfs::block_inventory()
                .into_iter()
                .find(|entry| entry.name == name)
                .ok_or(VfsError::NotFound)?;
            format!("../../block/{}", relative_path(&entry))
        };
        Ok(
            SimpleFile::new(self.fs.clone(), NodeType::Symlink, move || {
                Ok(target.clone())
            })
            .into(),
        )
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

struct BlockRootOps {
    fs: Arc<SimpleFs>,
}

impl SimpleDirOps for BlockRootOps {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let mut names = Vec::new();
        for number in 0..16 {
            names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            names.push(format!("loop{number}"));
        }
        for entry in axfs::block_inventory()
            .into_iter()
            .filter(|entry| entry.partition.is_none())
        {
            names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            names.push(entry.name);
        }
        owned_names(names)
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let name = core::str::from_utf8(name.as_bytes()).map_err(|_| VfsError::NotFound)?;
        if let Some(number) = name
            .strip_prefix("loop")
            .and_then(|n| n.parse::<u32>().ok())
        {
            if number >= 16 {
                return Err(VfsError::NotFound);
            }
            return Ok(NodeOpsMux::Dir(super::sys::loop_block_device_dir(
                self.fs.clone(),
                number,
                String::from(name),
                DeviceId::new(7, number),
            )));
        }
        let entry = axfs::block_inventory()
            .into_iter()
            .find(|entry| entry.name == name && entry.partition.is_none())
            .ok_or(VfsError::NotFound)?;
        let id = device_id(name).ok_or(VfsError::NotFound)?;
        Ok(NodeOpsMux::Dir(super::sys::block_device_dir(
            self.fs.clone(),
            String::from(name),
            id,
            entry.info,
        )))
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

struct DevBlockRootOps {
    fs: Arc<SimpleFs>,
}

impl SimpleDirOps for DevBlockRootOps {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let mut names = Vec::new();
        for number in 0..16 {
            names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            names.push(format!("7:{number}"));
        }
        for entry in axfs::block_inventory() {
            if let Some(id) = device_id(&entry.name) {
                names.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                names.push(format!("{}:{}", id.major(), id.minor()));
            }
        }
        owned_names(names)
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let name = core::str::from_utf8(name.as_bytes()).map_err(|_| VfsError::NotFound)?;
        let (major, minor) = name.split_once(':').ok_or(VfsError::NotFound)?;
        let major = major.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let minor = minor.parse::<u32>().map_err(|_| VfsError::NotFound)?;
        let target = if major == 7 && minor < 16 {
            format!("../../block/loop{minor}")
        } else {
            let entry = axfs::block_inventory()
                .into_iter()
                .find(|entry| device_id(&entry.name) == Some(DeviceId::new(major, minor)))
                .ok_or(VfsError::NotFound)?;
            format!("../../block/{}", relative_path(&entry))
        };
        Ok(
            SimpleFile::new(self.fs.clone(), NodeType::Symlink, move || {
                Ok(target.clone())
            })
            .into(),
        )
    }

    fn is_cacheable(&self) -> bool {
        false
    }
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
    fn unknown_removability_is_not_a_fixed_disk_claim() {
        assert_eq!(removable_text(Some(false)).unwrap(), "0\n");
        assert_eq!(removable_text(Some(true)).unwrap(), "1\n");
        assert_eq!(
            removable_text(None),
            Err(axfs_ng_vfs::VfsError::OperationNotSupported)
        );
    }
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
                removable: Some(false),
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

    #[test]
    fn pci_storage_device_ids_are_stable_across_registry_changes() {
        assert_eq!(sd_device_minor("sda"), Some(16));
        assert_eq!(sd_device_minor("sda1"), Some(17));
        assert_eq!(sd_device_minor("sdaa"), Some(16 * 27));
        assert_eq!(mmc_device_minor("mmcblk2"), Some(16));
        assert_eq!(mmc_device_minor("mmcblk2p3"), Some(19));
        assert_eq!(nvme_device_minor("nvme0n1"), Some(0));
        assert_eq!(nvme_device_minor("nvme0n1p2"), Some(2));
        assert_eq!(nvme_device_minor("nvme1n1"), Some(1 << 20));
    }
}
