use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType};
use axsync::Mutex;
use hashbrown::HashMap;
use lazy_static::lazy_static;

/// A stable inode identity used for Linux ETXTBSY exclusion.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ExecutableKey {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Default)]
struct ExecutableCounts {
    active: usize,
    write_open: usize,
}

struct ExecutableTable {
    entries: HashMap<ExecutableKey, ExecutableCounts>,
    active_refs: usize,
    write_open_refs: usize,
}

const MAX_EXECUTABLE_IDENTITIES: usize = 65_536;
const MAX_ACTIVE_EXECUTABLE_REFS: usize = 65_536;
const MAX_WRITE_OPEN_REFS: usize = 65_536;

lazy_static! {
    static ref EXECUTABLES: Mutex<ExecutableTable> = Mutex::new(ExecutableTable {
        entries: HashMap::new(),
        active_refs: 0,
        write_open_refs: 0,
    });
}

pub(crate) fn key(loc: &Location) -> Option<ExecutableKey> {
    (loc.node_type() == NodeType::RegularFile).then_some(ExecutableKey {
        device: loc.mountpoint().device(),
        inode: loc.inode(),
    })
}

fn admit_identity(table: &mut ExecutableTable, key: ExecutableKey) -> AxResult<()> {
    if table.entries.contains_key(&key) {
        return Ok(());
    }
    if table.entries.len() >= MAX_EXECUTABLE_IDENTITIES {
        return Err(LinuxError::ENFILE.into());
    }
    table.entries.try_reserve(1).map_err(|_| AxError::NoMemory)
}

fn increment_active(table: &mut ExecutableTable, key: ExecutableKey) -> AxResult<()> {
    if table.active_refs >= MAX_ACTIVE_EXECUTABLE_REFS {
        return Err(AxError::NoMemory);
    }
    admit_identity(table, key)?;
    let counts = table.entries.entry(key).or_default();
    counts.active = counts.active.checked_add(1).ok_or(AxError::NoMemory)?;
    table.active_refs += 1;
    Ok(())
}

fn increment_write_open(table: &mut ExecutableTable, key: ExecutableKey) -> AxResult<()> {
    if table
        .entries
        .get(&key)
        .is_some_and(|counts| counts.active != 0)
    {
        return Err(LinuxError::ETXTBSY.into());
    }
    if table.write_open_refs >= MAX_WRITE_OPEN_REFS {
        return Err(LinuxError::ENFILE.into());
    }
    admit_identity(table, key)?;
    let counts = table.entries.entry(key).or_default();
    counts.write_open = counts.write_open.checked_add(1).ok_or(LinuxError::ENFILE)?;
    table.write_open_refs += 1;
    Ok(())
}

pub(crate) fn acquire(loc: &Location) -> AxResult<Option<ExecutableKey>> {
    let Some(key) = key(loc) else {
        return Ok(None);
    };
    let mut table = EXECUTABLES.lock();
    increment_active(&mut table, key)?;
    Ok(Some(key))
}

/// Atomically excludes write-open descriptions and publishes one active-exec
/// reference.  A single table lock removes the old ACTIVE/WRITE lock-order
/// inversion and closes the check-then-insert race.
pub(crate) fn acquire_if_not_write_open(loc: &Location) -> AxResult<Option<ExecutableKey>> {
    let Some(key) = key(loc) else {
        return Ok(None);
    };

    let mut table = EXECUTABLES.lock();
    if table
        .entries
        .get(&key)
        .is_some_and(|counts| counts.write_open != 0)
    {
        return Err(LinuxError::ETXTBSY.into());
    }
    increment_active(&mut table, key)?;
    Ok(Some(key))
}

/// Retains an already-admitted executable identity for fork.  This never
/// allocates: a process carrying the key necessarily owns an existing active
/// reference.  Missing state is reported instead of silently losing ETXTBSY
/// protection in the child.
pub(crate) fn retain(key: Option<ExecutableKey>) -> AxResult<Option<ExecutableKey>> {
    let Some(key) = key else {
        return Ok(None);
    };
    let mut table = EXECUTABLES.lock();
    if table.active_refs >= MAX_ACTIVE_EXECUTABLE_REFS {
        return Err(AxError::NoMemory);
    }
    let counts = table.entries.get_mut(&key).ok_or(AxError::BadState)?;
    counts.active = counts.active.checked_add(1).ok_or(AxError::NoMemory)?;
    table.active_refs += 1;
    Ok(Some(key))
}

pub(crate) fn release(key: Option<ExecutableKey>) {
    let Some(key) = key else {
        return;
    };
    {
        let mut table = EXECUTABLES.lock();
        let should_remove = {
            let Some(counts) = table.entries.get_mut(&key) else {
                return;
            };
            if counts.active == 0 {
                return;
            }
            counts.active -= 1;
            counts.active == 0 && counts.write_open == 0
        };
        table.active_refs -= 1;
        if should_remove {
            table.entries.remove(&key);
        }
    }
}

/// Atomically checks active execution and reserves one persistent write-open
/// reference.  Admission is explicit and globally bounded.
pub(crate) fn retain_write_open(loc: &Location) -> AxResult<Option<ExecutableKey>> {
    let Some(key) = key(loc) else {
        return Ok(None);
    };
    let mut table = EXECUTABLES.lock();
    increment_write_open(&mut table, key)?;
    Ok(Some(key))
}

pub(crate) fn release_write_open(key: Option<ExecutableKey>) {
    let Some(key) = key else {
        return;
    };
    {
        let mut table = EXECUTABLES.lock();
        let should_remove = {
            let Some(counts) = table.entries.get_mut(&key) else {
                return;
            };
            if counts.write_open == 0 {
                return;
            }
            counts.write_open -= 1;
            counts.active == 0 && counts.write_open == 0
        };
        table.write_open_refs -= 1;
        if should_remove {
            table.entries.remove(&key);
        }
    }
}

pub(crate) fn is_active(loc: &Location) -> bool {
    let Some(key) = key(loc) else {
        return false;
    };
    EXECUTABLES
        .lock()
        .entries
        .get(&key)
        .is_some_and(|counts| counts.active != 0)
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
    if EXECUTABLES
        .lock()
        .entries
        .get(&key)
        .is_some_and(|counts| counts.write_open != 0)
    {
        Err(LinuxError::ETXTBSY.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_table() -> ExecutableTable {
        ExecutableTable {
            entries: HashMap::new(),
            active_refs: 0,
            write_open_refs: 0,
        }
    }

    #[test]
    fn active_and_write_open_admission_share_one_atomic_table() {
        let key = ExecutableKey {
            device: u64::MAX - 1,
            inode: u64::MAX - 2,
        };
        let mut table = empty_table();

        increment_active(&mut table, key).unwrap();
        assert_eq!(
            increment_write_open(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.write_open_refs, 0);

        table.entries.get_mut(&key).unwrap().active = 0;
        table.active_refs = 0;
        increment_write_open(&mut table, key).unwrap();
        assert_eq!(table.entries.get(&key).unwrap().write_open, 1);
        assert_eq!(table.write_open_refs, 1);
    }
}
