use alloc::{string::String, vec::Vec};

use axfs_ng_vfs::Location;
use spin::Mutex;

#[derive(Clone)]
pub struct MountRecord {
    pub source: String,
    pub target: String,
    pub fs_type: String,
    pub flags: u32,
}

static MOUNT_RECORDS: Mutex<Vec<MountRecord>> = Mutex::new(Vec::new());

const MS_RDONLY: u32 = 0x1;
const MS_MANDLOCK: u32 = 0x40;
const MS_NOSYMFOLLOW: u32 = 0x100;
const ST_NOSYMFOLLOW: u32 = 0x2000;

pub fn snapshot() -> Vec<MountRecord> {
    MOUNT_RECORDS.lock().clone()
}

pub fn record(source: String, target: String, fs_type: String, flags: u32) {
    let mut records = MOUNT_RECORDS.lock();
    records.retain(|record| record.target != target);
    records.push(MountRecord {
        source,
        target,
        fs_type,
        flags,
    });
}

pub fn remove(target: &str) {
    MOUNT_RECORDS
        .lock()
        .retain(|record| record.target != target);
}

fn contains_path(record_target: &str, path: &str) -> bool {
    record_target == "/"
        || path == record_target
        || path
            .strip_prefix(record_target)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub fn effective_flags(path: &str) -> u32 {
    let records = MOUNT_RECORDS.lock();
    records
        .iter()
        .filter(|record| contains_path(&record.target, path))
        .max_by_key(|record| record.target.len())
        .map_or(0, |record| record.flags)
}

pub fn is_readonly(path: &str) -> bool {
    effective_flags(path) & MS_RDONLY != 0
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

pub fn statfs_mount_flags(path: &str, base_flags: u32) -> u32 {
    let mount_flags = effective_flags(path);
    let mut result = base_flags;
    if mount_flags & MS_RDONLY != 0 {
        result |= MS_RDONLY;
    }
    if mount_flags & MS_NOSYMFOLLOW != 0 {
        result |= ST_NOSYMFOLLOW;
    }
    result
}
