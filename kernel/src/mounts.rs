use alloc::{string::String, vec::Vec};

use spin::Mutex;

#[derive(Clone)]
pub struct MountRecord {
    pub source: String,
    pub target: String,
    pub fs_type: String,
}

static MOUNT_RECORDS: Mutex<Vec<MountRecord>> = Mutex::new(Vec::new());

pub fn snapshot() -> Vec<MountRecord> {
    MOUNT_RECORDS.lock().clone()
}

pub fn record(source: String, target: String, fs_type: String) {
    let mut records = MOUNT_RECORDS.lock();
    records.retain(|record| record.target != target);
    records.push(MountRecord {
        source,
        target,
        fs_type,
    });
}

pub fn remove(target: &str) {
    MOUNT_RECORDS
        .lock()
        .retain(|record| record.target != target);
}
