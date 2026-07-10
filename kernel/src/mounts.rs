use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    sync::Weak,
    vec::Vec,
};

use axfs_ng_vfs::{DeviceId, Location, NodeType};
use spin::Mutex;

use crate::time::wall_time;

#[derive(Clone)]
pub struct MountRecord {
    pub mount_id: u64,
    pub parent_id: u64,
    pub root: String,
    pub source: String,
    pub target: String,
    pub fs_type: String,
    pub data: String,
    pub dev: u64,
    pub flags: u32,
    pub expire_marked: bool,
}

static MOUNT_RECORDS: Mutex<Vec<MountRecord>> = Mutex::new(Vec::new());
static LINUX_DEVICE_IDS: Mutex<BTreeMap<u64, (DeviceId, Weak<()>)>> = Mutex::new(BTreeMap::new());

pub const ROOT_BLOCK_SOURCE: &str = "/dev/vda";
pub const ROOT_BLOCK_DEVICE_ID: DeviceId = DeviceId::new(8, 0);

const MS_RDONLY: u32 = 0x1;
const MS_NOSUID: u32 = 0x2;
const MS_NODEV: u32 = 0x4;
const MS_NOEXEC: u32 = 0x8;
const MS_REMOUNT: u32 = 0x20;
const MS_MANDLOCK: u32 = 0x40;
const MS_NOSYMFOLLOW: u32 = 0x100;
const MS_NOATIME: u32 = 0x400;
const MS_NODIRATIME: u32 = 0x800;
const MS_BIND: u32 = 0x1000;
const MS_REC: u32 = 0x4000;
const MS_RELATIME: u32 = 0x20_0000;
const MS_STRICTATIME: u32 = 0x100_0000;
const MS_UNBINDABLE: u32 = 1 << 17;
const MS_PRIVATE: u32 = 1 << 18;
const MS_SLAVE: u32 = 1 << 19;
const MS_SHARED: u32 = 1 << 20;
const ST_RELATIME: u32 = 0x1000;
const ST_NOSYMFOLLOW: u32 = 0x2000;
const RELATIME_MAX_AGE_SECS: u64 = 24 * 60 * 60;
const PROPAGATION_FLAGS: u32 = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;

pub fn snapshot() -> Vec<MountRecord> {
    MOUNT_RECORDS.lock().clone()
}

pub fn register_linux_device(vfs_device: u64, linux_device: DeviceId, lifetime: Weak<()>) {
    if vfs_device != 0 {
        LINUX_DEVICE_IDS
            .lock()
            .insert(vfs_device, (linux_device, lifetime));
    }
}

pub fn linux_device_id(vfs_device: u64) -> DeviceId {
    if vfs_device == 0 {
        return DeviceId::default();
    }

    let mut devices = LINUX_DEVICE_IDS.lock();
    devices.retain(|_, (_, lifetime)| lifetime.strong_count() != 0);
    if let Some((device, _)) = devices.get(&vfs_device) {
        return *device;
    }
    drop(devices);

    let minor = vfs_device as u32;
    DeviceId::new(0, if minor == 0 { u32::MAX } else { minor })
}

pub fn extra_block_device_id(index: usize) -> Option<DeviceId> {
    let minor = index.checked_add(1)?.checked_mul(16)?;
    Some(DeviceId::new(8, u32::try_from(minor).ok()?))
}

pub fn record(
    source: String,
    target: String,
    fs_type: String,
    root: String,
    vfs_device: u64,
    mount_id: u64,
    parent_id: u64,
    flags: u32,
) {
    record_with_data(
        source,
        target,
        fs_type,
        root,
        vfs_device,
        mount_id,
        parent_id,
        flags,
        String::new(),
    );
}

pub fn record_with_data(
    source: String,
    target: String,
    fs_type: String,
    root: String,
    vfs_device: u64,
    mount_id: u64,
    parent_id: u64,
    flags: u32,
    data: String,
) {
    let mut records = MOUNT_RECORDS.lock();
    records.push(MountRecord {
        mount_id,
        parent_id,
        root,
        source,
        target,
        fs_type,
        data,
        dev: linux_device_id(vfs_device).0,
        flags,
        expire_marked: false,
    });
}

pub fn has_record(target: &str) -> bool {
    MOUNT_RECORDS
        .lock()
        .iter()
        .any(|record| record.target == target)
}

pub fn records_under(path: &str) -> Vec<MountRecord> {
    let records = MOUNT_RECORDS.lock();
    records
        .iter()
        .filter(|record| record.target != path && contains_path(path, &record.target))
        .cloned()
        .collect()
}

pub fn remount_with_data(
    source: String,
    target: String,
    fs_type: String,
    flags: u32,
    data: String,
) -> bool {
    let mut records = MOUNT_RECORDS.lock();
    if let Some(record) = records
        .iter_mut()
        .rev()
        .find(|record| record.target == target)
    {
        if !source.is_empty() {
            record.source = source;
        }
        if !fs_type.is_empty() {
            record.fs_type = fs_type;
        }
        if !data.is_empty() {
            record.data = data;
        }
        record.flags = flags;
        record.expire_marked = false;
        return true;
    }
    false
}

pub fn update_flags_for_path(path: &str, flags: u32) -> bool {
    let mut records = MOUNT_RECORDS.lock();
    let Some((_, record)) = records
        .iter_mut()
        .enumerate()
        .filter(|(_, record)| contains_path(&record.target, path))
        .max_by_key(|(index, record)| (record.target.len(), *index))
    else {
        return false;
    };
    record.flags = flags;
    record.expire_marked = false;
    true
}

pub fn change_propagation(target: &str, flags: u32, recursive: bool) {
    let propagation = flags & PROPAGATION_FLAGS;
    if propagation == 0 {
        return;
    }

    let mut records = MOUNT_RECORDS.lock();
    for record in records.iter_mut() {
        if record.target == target || (recursive && contains_path(target, &record.target)) {
            record.flags = (record.flags & !PROPAGATION_FLAGS) | propagation | (flags & MS_REC);
            record.expire_marked = false;
        }
    }
}

pub fn move_tree(root_mount_id: u64, old_target: &str, new_target: &str, new_parent_id: u64) {
    let mut records = MOUNT_RECORDS.lock();
    move_tree_records(
        &mut records,
        root_mount_id,
        old_target,
        new_target,
        new_parent_id,
    );
}

fn move_tree_records(
    records: &mut [MountRecord],
    root_mount_id: u64,
    old_target: &str,
    new_target: &str,
    new_parent_id: u64,
) {
    let subtree = subtree_mount_ids(records, root_mount_id);
    for record in records.iter_mut() {
        if !subtree.contains(&record.mount_id) {
            continue;
        }

        if let Some(suffix) = path_suffix(old_target, &record.target) {
            record.target = joined_path(new_target, suffix);
            record.expire_marked = false;
        }
        if record.mount_id == root_mount_id {
            record.parent_id = new_parent_id;
        }
    }
}

fn subtree_mount_ids(records: &[MountRecord], root_mount_id: u64) -> BTreeSet<u64> {
    let mut ids = BTreeSet::new();
    ids.insert(root_mount_id);

    loop {
        let old_len = ids.len();
        for record in records {
            if ids.contains(&record.parent_id) {
                ids.insert(record.mount_id);
            }
        }
        if ids.len() == old_len {
            return ids;
        }
    }
}

pub fn remove_subtree(root_mount_id: u64) -> Vec<MountRecord> {
    let mut records = MOUNT_RECORDS.lock();
    let ids = subtree_mount_ids(&records, root_mount_id);
    let mut removed = Vec::new();
    let mut index = 0;
    while index < records.len() {
        if ids.contains(&records[index].mount_id) {
            removed.push(records.remove(index));
        } else {
            index += 1;
        }
    }
    removed
}

pub fn mark_expiry(target: &str) -> bool {
    let mut records = MOUNT_RECORDS.lock();
    let Some(record) = records
        .iter_mut()
        .rev()
        .find(|record| record.target == target)
    else {
        return false;
    };
    let was_marked = record.expire_marked;
    record.expire_marked = true;
    was_marked
}

pub fn clear_expiry_for_path(path: &str) {
    for record in MOUNT_RECORDS.lock().iter_mut() {
        if contains_path(&record.target, path) {
            record.expire_marked = false;
        }
    }
}

fn contains_path(record_target: &str, path: &str) -> bool {
    record_target == "/"
        || path == record_target
        || path
            .strip_prefix(record_target)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_suffix<'a>(base: &str, path: &'a str) -> Option<&'a str> {
    if path == base {
        Some("")
    } else if base == "/" && path.starts_with('/') {
        Some(path)
    } else {
        path.strip_prefix(base)
            .filter(|suffix| suffix.starts_with('/'))
    }
}

fn joined_path(base: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        base.to_string()
    } else if base == "/" {
        suffix.to_string()
    } else {
        alloc::format!("{base}{suffix}")
    }
}

pub fn shared_aliases_for(path: &str) -> Vec<String> {
    let records = snapshot();
    let mut seen = BTreeSet::new();
    let mut queue = Vec::new();

    seen.insert(path.to_string());
    queue.push(path.to_string());

    let mut index = 0;
    while index < queue.len() {
        let current = queue[index].clone();
        index += 1;

        for record in &records {
            if record.flags & MS_BIND == 0
                || record.flags & MS_SHARED == 0
                || record.flags & MS_UNBINDABLE != 0
            {
                continue;
            }
            if let Some(suffix) = path_suffix(&record.source, &current) {
                let alias = joined_path(&record.target, suffix);
                if seen.insert(alias.clone()) {
                    queue.push(alias);
                }
            }
            if let Some(suffix) = path_suffix(&record.target, &current) {
                let alias = joined_path(&record.source, suffix);
                if seen.insert(alias.clone()) {
                    queue.push(alias);
                }
            }
        }
    }

    seen.into_iter().filter(|alias| alias != path).collect()
}

pub fn effective_flags(path: &str) -> u32 {
    let records = MOUNT_RECORDS.lock();
    records
        .iter()
        .enumerate()
        .filter(|(_, record)| contains_path(&record.target, path))
        .max_by_key(|(index, record)| (record.target.len(), *index))
        .map(|(_, record)| record)
        .map_or(0, |record| record.flags)
}

pub fn is_readonly(path: &str) -> bool {
    effective_flags(path) & MS_RDONLY != 0
}

pub fn is_nodev(path: &str) -> bool {
    effective_flags(path) & MS_NODEV != 0
}

pub fn is_noexec(path: &str) -> bool {
    effective_flags(path) & MS_NOEXEC != 0
}

pub fn has_mandatory_locking(path: &str) -> bool {
    effective_flags(path) & MS_MANDLOCK != 0
}

pub fn should_follow_symlink(loc: &Location) -> bool {
    let Ok(path) = loc.absolute_path() else {
        return true;
    };
    effective_flags(path.as_ref()) & MS_NOSYMFOLLOW == 0
}

pub fn should_update_atime(loc: &Location) -> bool {
    let Ok(path) = loc.absolute_path() else {
        return true;
    };
    let flags = effective_flags(path.as_ref());
    if flags & MS_NOATIME != 0 {
        return false;
    }

    let metadata = match loc.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return true,
    };
    if flags & MS_NODIRATIME != 0 && metadata.node_type == NodeType::Directory {
        return false;
    }
    if flags & MS_STRICTATIME != 0 {
        return true;
    }

    let relatime = flags & MS_RELATIME != 0 || flags == 0;
    if !relatime {
        return true;
    }

    let now = wall_time();
    metadata.atime <= metadata.mtime
        || metadata.atime <= metadata.ctime
        || now.saturating_sub(metadata.atime).as_secs() >= RELATIME_MAX_AGE_SECS
}

pub fn mount_options(flags: u32) -> String {
    let mut options = Vec::new();
    options.push(if flags & MS_RDONLY != 0 { "ro" } else { "rw" });
    if flags & MS_BIND != 0 {
        options.push("bind");
    }
    if flags & MS_REC != 0 {
        options.push("rbind");
    }
    if flags & MS_NOSUID != 0 {
        options.push("nosuid");
    }
    if flags & MS_NODEV != 0 {
        options.push("nodev");
    }
    if flags & MS_NOEXEC != 0 {
        options.push("noexec");
    }
    if flags & MS_MANDLOCK != 0 {
        options.push("mand");
    }
    if flags & MS_NOSYMFOLLOW != 0 {
        options.push("nosymfollow");
    }
    if flags & MS_NOATIME != 0 {
        options.push("noatime");
    } else if flags & MS_STRICTATIME != 0 {
        options.push("strictatime");
    } else {
        options.push("relatime");
    }
    if flags & MS_NODIRATIME != 0 {
        options.push("nodiratime");
    }
    if flags & MS_SHARED != 0 {
        options.push("shared");
    } else if flags & MS_SLAVE != 0 {
        options.push("slave");
    } else if flags & MS_PRIVATE != 0 {
        options.push("private");
    } else if flags & MS_UNBINDABLE != 0 {
        options.push("unbindable");
    }
    options.join(",")
}

pub fn statfs_mount_flags(path: &str, base_flags: u32) -> u32 {
    let mount_flags = effective_flags(path);
    let mut result = base_flags;
    result |= mount_flags
        & (MS_RDONLY
            | MS_NOSUID
            | MS_NODEV
            | MS_NOEXEC
            | MS_REMOUNT
            | MS_MANDLOCK
            | MS_NOATIME
            | MS_NODIRATIME);
    if mount_flags & MS_RELATIME != 0 {
        result |= ST_RELATIME;
    }
    if mount_flags & MS_NOSYMFOLLOW != 0 {
        result |= ST_NOSYMFOLLOW;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(mount_id: u64, parent_id: u64, target: &str) -> MountRecord {
        MountRecord {
            mount_id,
            parent_id,
            root: "/".to_string(),
            source: "none".to_string(),
            target: target.to_string(),
            fs_type: "tmpfs".to_string(),
            data: String::new(),
            dev: mount_id,
            flags: 0,
            expire_marked: false,
        }
    }

    #[test]
    fn mount_subtree_uses_ids_instead_of_stacked_paths() {
        let records = [
            record(1, 0, "/"),
            record(2, 1, "/mnt"),
            record(3, 2, "/mnt"),
            record(4, 3, "/mnt/nested"),
            record(5, 1, "/other"),
        ];

        let ids = subtree_mount_ids(&records, 3);
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), [3, 4]);
    }

    #[test]
    fn move_tree_only_moves_the_selected_stacked_subtree() {
        let mut records = [
            record(1, 0, "/"),
            record(2, 1, "/mnt"),
            record(3, 2, "/mnt"),
            record(4, 3, "/mnt/nested"),
        ];

        move_tree_records(&mut records, 3, "/mnt", "/moved", 1);

        let lower = records.iter().find(|record| record.mount_id == 2).unwrap();
        let moved = records.iter().find(|record| record.mount_id == 3).unwrap();
        let nested = records.iter().find(|record| record.mount_id == 4).unwrap();
        assert_eq!(lower.target, "/mnt");
        assert_eq!(lower.parent_id, 1);
        assert_eq!(moved.target, "/moved");
        assert_eq!(moved.parent_id, 1);
        assert_eq!(nested.target, "/moved/nested");
        assert_eq!(nested.parent_id, 3);
    }

    #[test]
    fn move_tree_rewrites_descendants_of_a_root_overmount() {
        let mut records = [
            record(1, 0, "/"),
            record(2, 1, "/"),
            record(3, 2, "/nested"),
        ];

        move_tree_records(&mut records, 2, "/", "/moved", 1);

        let namespace_root = records.iter().find(|record| record.mount_id == 1).unwrap();
        let moved = records.iter().find(|record| record.mount_id == 2).unwrap();
        let nested = records.iter().find(|record| record.mount_id == 3).unwrap();
        assert_eq!(namespace_root.target, "/");
        assert_eq!(moved.target, "/moved");
        assert_eq!(nested.target, "/moved/nested");
    }

    #[test]
    fn move_tree_to_namespace_root_does_not_add_a_second_separator() {
        let mut records = [
            record(1, 0, "/"),
            record(2, 1, "/source"),
            record(3, 2, "/source/nested"),
        ];

        move_tree_records(&mut records, 2, "/source", "/", 1);

        let moved = records.iter().find(|record| record.mount_id == 2).unwrap();
        let nested = records.iter().find(|record| record.mount_id == 3).unwrap();
        assert_eq!(moved.target, "/");
        assert_eq!(nested.target, "/nested");
    }
}
