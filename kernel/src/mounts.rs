use alloc::{string::String, vec::Vec};

use axfs_ng_vfs::{Location, NodeType};
use spin::Mutex;

use crate::time::wall_time;

#[derive(Clone)]
pub struct MountRecord {
    pub source: String,
    pub target: String,
    pub fs_type: String,
    pub flags: u32,
}

static MOUNT_RECORDS: Mutex<Vec<MountRecord>> = Mutex::new(Vec::new());

const MS_RDONLY: u32 = 0x1;
const MS_NOSUID: u32 = 0x2;
const MS_NODEV: u32 = 0x4;
const MS_NOEXEC: u32 = 0x8;
const MS_REMOUNT: u32 = 0x20;
const MS_MANDLOCK: u32 = 0x40;
const MS_NOSYMFOLLOW: u32 = 0x100;
const MS_NOATIME: u32 = 0x400;
const MS_NODIRATIME: u32 = 0x800;
const MS_RELATIME: u32 = 0x20_0000;
const MS_STRICTATIME: u32 = 0x100_0000;
const ST_RELATIME: u32 = 0x1000;
const ST_NOSYMFOLLOW: u32 = 0x2000;
const RELATIME_MAX_AGE_SECS: u64 = 24 * 60 * 60;

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

pub fn remount(source: String, target: String, fs_type: String, flags: u32) {
    let mut records = MOUNT_RECORDS.lock();
    if let Some(record) = records.iter_mut().find(|record| record.target == target) {
        if !source.is_empty() {
            record.source = source;
        }
        if !fs_type.is_empty() {
            record.fs_type = fs_type;
        }
        record.flags = flags;
        return;
    }
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
