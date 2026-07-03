use alloc::collections::BTreeMap;

use axerrno::{AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType};
use axsync::Mutex;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ExecutableKey {
    device: u64,
    inode: u64,
}

static ACTIVE_EXECUTABLES: Mutex<BTreeMap<ExecutableKey, usize>> = Mutex::new(BTreeMap::new());
static WRITE_OPEN_FILES: Mutex<BTreeMap<ExecutableKey, usize>> = Mutex::new(BTreeMap::new());

pub(crate) fn key(loc: &Location) -> Option<ExecutableKey> {
    (loc.node_type() == NodeType::RegularFile).then_some(ExecutableKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    })
}

pub(crate) fn acquire(loc: &Location) -> Option<ExecutableKey> {
    retain(key(loc))
}

pub(crate) fn acquire_if_not_write_open(loc: &Location) -> AxResult<Option<ExecutableKey>> {
    let Some(key) = key(loc) else {
        return Ok(None);
    };

    let mut active = ACTIVE_EXECUTABLES.lock();
    if WRITE_OPEN_FILES
        .lock()
        .get(&key)
        .is_some_and(|count| *count != 0)
    {
        return Err(LinuxError::ETXTBSY.into());
    }

    *active.entry(key).or_insert(0) += 1;
    Ok(Some(key))
}

pub(crate) fn retain(key: Option<ExecutableKey>) -> Option<ExecutableKey> {
    let key = key?;
    let mut active = ACTIVE_EXECUTABLES.lock();
    *active.entry(key).or_insert(0) += 1;
    Some(key)
}

pub(crate) fn release(key: Option<ExecutableKey>) {
    let Some(key) = key else {
        return;
    };
    let mut active = ACTIVE_EXECUTABLES.lock();
    let Some(count) = active.get_mut(&key) else {
        return;
    };
    *count = count.saturating_sub(1);
    if *count == 0 {
        active.remove(&key);
    }
}

pub(crate) fn retain_write_open(loc: &Location) -> Option<ExecutableKey> {
    let key = key(loc)?;
    let mut open = WRITE_OPEN_FILES.lock();
    *open.entry(key).or_insert(0) += 1;
    Some(key)
}

pub(crate) fn release_write_open(key: Option<ExecutableKey>) {
    let Some(key) = key else {
        return;
    };
    let mut open = WRITE_OPEN_FILES.lock();
    let Some(count) = open.get_mut(&key) else {
        return;
    };
    *count = count.saturating_sub(1);
    if *count == 0 {
        open.remove(&key);
    }
}

pub(crate) fn is_active(loc: &Location) -> bool {
    let Some(key) = key(loc) else {
        return false;
    };
    ACTIVE_EXECUTABLES
        .lock()
        .get(&key)
        .is_some_and(|count| *count != 0)
}

pub(crate) fn check_not_active(loc: &Location) -> AxResult<()> {
    if is_active(loc) {
        Err(LinuxError::ETXTBSY.into())
    } else {
        Ok(())
    }
}

pub(crate) fn check_not_write_open(loc: &Location) -> AxResult<()> {
    let Some(key) = key(loc) else {
        return Ok(());
    };
    if WRITE_OPEN_FILES
        .lock()
        .get(&key)
        .is_some_and(|count| *count != 0)
    {
        Err(LinuxError::ETXTBSY.into())
    } else {
        Ok(())
    }
}
