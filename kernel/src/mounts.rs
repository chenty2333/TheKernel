use alloc::{string::String, vec::Vec};

use spin::Mutex;

#[derive(Clone)]
pub struct MountRecord {
    pub source: String,
    pub target: String,
    pub fs_type: String,
    pub flags: u32,
}

static MOUNT_RECORDS: Mutex<Vec<MountRecord>> = Mutex::new(Vec::new());

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

pub fn is_readonly(path: &str) -> bool {
    let records = MOUNT_RECORDS.lock();
    records
        .iter()
        .filter(|record| record.flags & 1 != 0)
        .filter(|record| {
            record.target == "/"
                || path == record.target
                || path
                    .strip_prefix(record.target.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .max_by_key(|record| record.target.len())
        .is_some()
}
