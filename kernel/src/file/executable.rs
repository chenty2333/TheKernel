use core::sync::atomic::{AtomicBool, Ordering};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, NodeType};
#[cfg(not(test))]
use axsync::Mutex;
use hashbrown::HashMap;
#[cfg(test)]
use spin::Mutex;
use spin::Once;

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
    writable_mappings: usize,
    credential_reads: usize,
    credential_writes: usize,
}

struct ExecutableTable {
    entries: HashMap<ExecutableKey, ExecutableCounts>,
    identity_limit: usize,
    active_refs: usize,
    write_open_refs: usize,
    writable_mapping_refs: usize,
    credential_read_refs: usize,
    credential_write_refs: usize,
}

struct RetiredExecutableEntry {
    _key: ExecutableKey,
    _counts: ExecutableCounts,
}

struct ReleaseOutcome {
    released: bool,
    retired: Option<RetiredExecutableEntry>,
}

const MAX_EXECUTABLE_IDENTITIES: usize = 8_192;
const PREALLOCATED_CAPACITY: usize = 2 * MAX_EXECUTABLE_IDENTITIES;
const MAX_ACTIVE_EXECUTABLE_REFS: usize = 65_536;
const MAX_WRITE_OPEN_REFS: usize = 65_536;
const MAX_WRITABLE_MAPPING_REFS: usize = 65_536;
const MAX_CREDENTIAL_READ_REFS: usize = 65_536;
const MAX_CREDENTIAL_WRITE_REFS: usize = 65_536;

static EXECUTABLES: Once<Mutex<ExecutableTable>> = Once::new();

impl ExecutableTable {
    fn try_new(identity_limit: usize, preallocated_capacity: usize) -> AxResult<Self> {
        // hashbrown can reclaim tombstones without resizing whenever the
        // post-insert length is at most half of its full capacity. Reserving
        // twice the identity ceiling up front therefore keeps every later
        // entry insertion and in-place rehash allocator-free.
        let required_capacity = identity_limit.checked_mul(2).ok_or(AxError::NoMemory)?;
        if preallocated_capacity < required_capacity {
            return Err(AxError::BadState);
        }
        let mut entries = HashMap::new();
        entries
            .try_reserve(preallocated_capacity)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            entries,
            identity_limit,
            active_refs: 0,
            write_open_refs: 0,
            writable_mapping_refs: 0,
            credential_read_refs: 0,
            credential_write_refs: 0,
        })
    }
}

/// Fallibly allocates the complete executable registry before any loader or
/// process can publish a lease. Runtime users never initialize this lazily.
pub(crate) fn init() -> AxResult<()> {
    EXECUTABLES
        .try_call_once(|| {
            ExecutableTable::try_new(MAX_EXECUTABLE_IDENTITIES, PREALLOCATED_CAPACITY)
                .map(Mutex::new)
        })
        .map(|_| ())
}

fn executables() -> AxResult<&'static Mutex<ExecutableTable>> {
    EXECUTABLES.get().ok_or(AxError::BadState)
}

fn retire_if_unused(
    table: &mut ExecutableTable,
    key: ExecutableKey,
) -> Option<RetiredExecutableEntry> {
    let unused = table.entries.get(&key).is_some_and(|counts| {
        counts.active == 0
            && counts.write_open == 0
            && counts.writable_mappings == 0
            && counts.credential_reads == 0
            && counts.credential_writes == 0
    });
    unused
        .then(|| table.entries.remove_entry(&key))
        .flatten()
        .map(|(key, counts)| RetiredExecutableEntry {
            _key: key,
            _counts: counts,
        })
}

fn release_active_in(table: &mut ExecutableTable, key: ExecutableKey) -> ReleaseOutcome {
    let Some(counts) = table.entries.get_mut(&key) else {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    };
    if counts.active == 0 {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    }
    counts.active -= 1;
    table.active_refs -= 1;
    ReleaseOutcome {
        released: true,
        retired: retire_if_unused(table, key),
    }
}

fn release_write_open_in(table: &mut ExecutableTable, key: ExecutableKey) -> ReleaseOutcome {
    let Some(counts) = table.entries.get_mut(&key) else {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    };
    if counts.write_open == 0 {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    }
    counts.write_open -= 1;
    table.write_open_refs -= 1;
    ReleaseOutcome {
        released: true,
        retired: retire_if_unused(table, key),
    }
}

fn release_writable_mapping_in(table: &mut ExecutableTable, key: ExecutableKey) -> ReleaseOutcome {
    let Some(counts) = table.entries.get_mut(&key) else {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    };
    if counts.writable_mappings == 0 {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    }
    counts.writable_mappings -= 1;
    table.writable_mapping_refs -= 1;
    ReleaseOutcome {
        released: true,
        retired: retire_if_unused(table, key),
    }
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
    if table.entries.len() >= table.identity_limit {
        return Err(LinuxError::ENFILE.into());
    }
    Ok(())
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
        .is_some_and(|counts| counts.active != 0 || counts.credential_writes != 0)
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

fn increment_writable_mapping(table: &mut ExecutableTable, key: ExecutableKey) -> AxResult<()> {
    if table.entries.get(&key).is_some_and(|counts| {
        counts.active != 0 || counts.credential_reads != 0 || counts.credential_writes != 0
    }) {
        return Err(LinuxError::ETXTBSY.into());
    }
    if table.writable_mapping_refs >= MAX_WRITABLE_MAPPING_REFS {
        return Err(LinuxError::ENFILE.into());
    }
    admit_identity(table, key)?;
    let counts = table.entries.entry(key).or_default();
    counts.writable_mappings = counts
        .writable_mappings
        .checked_add(1)
        .ok_or(LinuxError::ENFILE)?;
    table.writable_mapping_refs += 1;
    Ok(())
}

pub(crate) fn acquire(loc: &Location) -> AxResult<Option<ExecutableKey>> {
    let executables = executables()?;
    let Some(key) = key(loc) else {
        return Ok(None);
    };
    let mut table = executables.lock();
    increment_active(&mut table, key)?;
    Ok(Some(key))
}

/// Retains an already-admitted executable identity for fork.  This never
/// allocates: a process carrying the key necessarily owns an existing active
/// reference.  Missing state is reported instead of silently losing ETXTBSY
/// protection in the child.
pub(crate) fn retain(key: Option<ExecutableKey>) -> AxResult<Option<ExecutableKey>> {
    let executables = executables()?;
    let Some(key) = key else {
        return Ok(None);
    };
    let mut table = executables.lock();
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
    let Some(executables) = EXECUTABLES.get() else {
        error!("executable active release ran before registry initialization");
        return;
    };
    let outcome = {
        let mut table = executables.lock();
        release_active_in(&mut table, key)
    };
    if !outcome.released {
        error!("executable active release lost its accounted reference");
    }
    drop(outcome.retired);
}

/// Atomically checks active execution and reserves one persistent write-open
/// reference.  Admission is explicit and globally bounded.
pub(crate) fn retain_write_open(loc: &Location) -> AxResult<Option<ExecutableKey>> {
    let executables = executables()?;
    let Some(key) = key(loc) else {
        return Ok(None);
    };
    let mut table = executables.lock();
    increment_write_open(&mut table, key)?;
    Ok(Some(key))
}

pub(crate) fn release_write_open(key: Option<ExecutableKey>) {
    let Some(key) = key else {
        return;
    };
    let Some(executables) = EXECUTABLES.get() else {
        error!("executable write-open release ran before registry initialization");
        return;
    };
    let outcome = {
        let mut table = executables.lock();
        release_write_open_in(&mut table, key)
    };
    if !outcome.released {
        error!("executable write-open release lost its accounted reference");
    }
    drop(outcome.retired);
}

fn is_active(loc: &Location) -> AxResult<bool> {
    let executables = executables()?;
    let Some(key) = key(loc) else {
        return Ok(false);
    };
    Ok(executables
        .lock()
        .entries
        .get(&key)
        .is_some_and(|counts| counts.active != 0))
}

fn increment_credential_read(table: &mut ExecutableTable, key: ExecutableKey) -> AxResult<()> {
    if table.entries.get(&key).is_some_and(|counts| {
        counts.write_open != 0 || counts.writable_mappings != 0 || counts.credential_writes != 0
    }) {
        return Err(LinuxError::ETXTBSY.into());
    }
    if table.active_refs >= MAX_ACTIVE_EXECUTABLE_REFS
        || table.credential_read_refs >= MAX_CREDENTIAL_READ_REFS
    {
        return Err(AxError::NoMemory);
    }
    admit_identity(table, key)?;
    let counts = table.entries.entry(key).or_default();
    counts.active = counts.active.checked_add(1).ok_or(AxError::NoMemory)?;
    counts.credential_reads = counts
        .credential_reads
        .checked_add(1)
        .ok_or(AxError::NoMemory)?;
    table.active_refs += 1;
    table.credential_read_refs += 1;
    Ok(())
}

fn release_credential_read_in(table: &mut ExecutableTable, key: ExecutableKey) -> ReleaseOutcome {
    let Some(counts) = table.entries.get_mut(&key) else {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    };
    if counts.active == 0 || counts.credential_reads == 0 {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    }
    counts.active -= 1;
    counts.credential_reads -= 1;
    table.active_refs -= 1;
    table.credential_read_refs -= 1;
    ReleaseOutcome {
        released: true,
        retired: retire_if_unused(table, key),
    }
}

fn finish_credential_read_in(table: &mut ExecutableTable, key: ExecutableKey) -> bool {
    let Some(counts) = table.entries.get_mut(&key) else {
        return false;
    };
    if counts.active == 0 || counts.credential_reads == 0 {
        return false;
    }
    counts.credential_reads -= 1;
    table.credential_read_refs -= 1;
    true
}

fn increment_credential_write(
    table: &mut ExecutableTable,
    key: ExecutableKey,
    exclude_content_writers: bool,
) -> AxResult<()> {
    if table.entries.get(&key).is_some_and(|counts| {
        counts.credential_reads != 0
            || counts.credential_writes != 0
            || (exclude_content_writers
                && (counts.write_open != 0 || counts.writable_mappings != 0))
    }) {
        return Err(LinuxError::ETXTBSY.into());
    }
    if table.credential_write_refs >= MAX_CREDENTIAL_WRITE_REFS {
        return Err(LinuxError::ENFILE.into());
    }
    admit_identity(table, key)?;
    let counts = table.entries.entry(key).or_default();
    counts.credential_writes = counts
        .credential_writes
        .checked_add(1)
        .ok_or(LinuxError::ENFILE)?;
    table.credential_write_refs += 1;
    Ok(())
}

fn release_credential_write_in(table: &mut ExecutableTable, key: ExecutableKey) -> ReleaseOutcome {
    let Some(counts) = table.entries.get_mut(&key) else {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    };
    if counts.credential_writes == 0 {
        return ReleaseOutcome {
            released: false,
            retired: None,
        };
    }
    counts.credential_writes -= 1;
    table.credential_write_refs -= 1;
    ReleaseOutcome {
        released: true,
        retired: retire_if_unused(table, key),
    }
}

/// Pins the final executable's content and privilege metadata while exec
/// derives and authorizes its replacement credential.
///
/// The lease owns one active-executable reference as well, so content writers
/// remain excluded. Metadata writers use [`with_credential_metadata_unpinned`]
/// and therefore cannot race set-ID/file-capability sampling through commit.
pub(crate) struct CredentialReadLease {
    key: Option<ExecutableKey>,
}

impl CredentialReadLease {
    pub(crate) fn acquire(loc: &Location) -> AxResult<Self> {
        let executables = executables()?;
        let Some(key) = key(loc) else {
            return Ok(Self { key: None });
        };
        let mut table = executables.lock();
        increment_credential_read(&mut table, key)?;
        Ok(Self { key: Some(key) })
    }

    /// Converts the transient credential lease into the process image's
    /// persistent active-executable reference.
    ///
    /// Callers perform this immediately after composite image publication. A
    /// missing count is reported instead of becoming a debug-only assertion or
    /// silent success; the post-commit caller must fail the new process closed.
    pub(crate) fn finish(mut self) -> AxResult<Option<ExecutableKey>> {
        let Some(key) = self.key else {
            return Ok(None);
        };
        let mut table = executables()?.lock();
        if !finish_credential_read_in(&mut table, key) {
            return Err(AxError::BadState);
        }
        self.key = None;
        Ok(Some(key))
    }
}

impl Drop for CredentialReadLease {
    fn drop(&mut self) {
        let Some(key) = self.key.take() else {
            return;
        };
        let Some(executables) = EXECUTABLES.get() else {
            error!("executable credential-read release ran before registry initialization");
            return;
        };
        let outcome = {
            let mut table = executables.lock();
            release_credential_read_in(&mut table, key)
        };
        if !outcome.released {
            error!("executable credential-read lease lost its accounted reference");
        }
        drop(outcome.retired);
    }
}

/// Runs a privilege-metadata mutation while atomically excluding an in-flight
/// exec credential snapshot. Persistent execution alone does not block chmod,
/// chown, or file-capability changes after exec has committed.
pub(crate) fn with_credential_metadata_unpinned<T>(
    loc: &Location,
    operation: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    let Some(lease) = CredentialWriteLease::acquire(loc, false)? else {
        return operation();
    };
    let result = operation();
    drop(lease);
    result
}

/// Runs a file-capability set/remove transaction while excluding both exec
/// credential sampling and every admitted content writer. This is deliberately
/// stricter than Linux while the VFS lacks a provider-neutral killpriv hook:
/// an already-open writer or active shared-writable mapping makes setcap fail
/// instead of relying on a later provider callback to revoke the new xattr.
pub(crate) fn with_file_capability_metadata_unpinned<T>(
    loc: &Location,
    operation: impl FnOnce() -> AxResult<T>,
) -> AxResult<T> {
    let Some(lease) = CredentialWriteLease::acquire(loc, true)? else {
        return operation();
    };
    let result = operation();
    drop(lease);
    result
}

/// Reserves an inode privilege-metadata mutation without holding the global
/// executable table across filesystem work. Credential readers reject this
/// reservation, and metadata writers reject an existing credential reader,
/// so the sampled mode/owner/xattr tuple stays stable through exec commit.
struct CredentialWriteLease {
    key: ExecutableKey,
}

impl CredentialWriteLease {
    fn acquire(loc: &Location, exclude_content_writers: bool) -> AxResult<Option<Self>> {
        let executables = executables()?;
        let Some(key) = key(loc) else {
            return Ok(None);
        };
        let mut table = executables.lock();
        increment_credential_write(&mut table, key, exclude_content_writers)?;
        Ok(Some(Self { key }))
    }
}

impl Drop for CredentialWriteLease {
    fn drop(&mut self) {
        let Some(executables) = EXECUTABLES.get() else {
            error!("executable credential-write release ran before registry initialization");
            return;
        };
        let outcome = {
            let mut table = executables.lock();
            release_credential_write_in(&mut table, self.key)
        };
        if !outcome.released {
            error!("executable credential-write lease lost its accounted reference");
        }
        drop(outcome.retired);
    }
}

/// Per-file-backend admission for a shared writable mapping.
///
/// The token itself allocates nothing. It publishes one globally bounded
/// reference only while its backend is writable, so setcap and exec can
/// atomically reject a mapping even after the original writable fd is closed.
pub(crate) struct WritableMappingRegistration {
    key: ExecutableKey,
    active: AtomicBool,
}

impl WritableMappingRegistration {
    pub(crate) fn for_location(loc: &Location) -> Option<Self> {
        Some(Self {
            key: key(loc)?,
            active: AtomicBool::new(false),
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn set_active(&self, active: bool) -> AxResult<()> {
        let executables = executables()?;
        let mut table = executables.lock();
        let previous = self.active.load(Ordering::Acquire);
        if previous == active {
            return Ok(());
        }
        if active {
            increment_writable_mapping(&mut table, self.key)?;
            self.active.store(true, Ordering::Release);
            return Ok(());
        }

        let outcome = release_writable_mapping_in(&mut table, self.key);
        if !outcome.released {
            return Err(AxError::BadState);
        }
        self.active.store(false, Ordering::Release);
        drop(table);
        drop(outcome.retired);
        Ok(())
    }
}

impl Drop for WritableMappingRegistration {
    fn drop(&mut self) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some(executables) = EXECUTABLES.get() else {
            error!("executable writable-mapping release ran before registry initialization");
            return;
        };
        let outcome = {
            let mut table = executables.lock();
            release_writable_mapping_in(&mut table, self.key)
        };
        self.active.store(false, Ordering::Release);
        if !outcome.released {
            error!("executable writable-mapping release lost its accounted reference");
        }
        drop(outcome.retired);
    }
}

pub(crate) fn check_not_active(loc: &Location) -> AxResult<()> {
    if is_active(loc)? {
        Err(LinuxError::ETXTBSY.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_table(identity_limit: usize) -> ExecutableTable {
        ExecutableTable::try_new(identity_limit, identity_limit * 2).unwrap()
    }

    fn test_key(value: u64) -> ExecutableKey {
        ExecutableKey {
            device: u64::MAX - value,
            inode: value,
        }
    }

    fn finish_release(outcome: ReleaseOutcome) -> bool {
        let released = outcome.released;
        drop(outcome.retired);
        released
    }

    #[test]
    fn initialization_fallibly_reserves_the_no_growth_capacity() {
        let table =
            ExecutableTable::try_new(MAX_EXECUTABLE_IDENTITIES, PREALLOCATED_CAPACITY).unwrap();
        assert!(table.entries.capacity() >= PREALLOCATED_CAPACITY);
        assert_eq!(table.identity_limit, MAX_EXECUTABLE_IDENTITIES);
        assert!(table.entries.is_empty());
        assert_eq!(table.active_refs, 0);
        assert_eq!(table.write_open_refs, 0);
        assert_eq!(table.writable_mapping_refs, 0);
        assert_eq!(table.credential_read_refs, 0);
        assert_eq!(table.credential_write_refs, 0);
        assert!(matches!(
            ExecutableTable::try_new(2, 3),
            Err(AxError::BadState)
        ));
    }

    #[test]
    fn active_and_write_open_admission_share_one_atomic_table() {
        let key = test_key(1);
        let mut table = test_table(4);

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

    #[test]
    fn write_open_and_credential_read_exclude_each_other_in_both_directions() {
        let key = test_key(2);
        let mut table = test_table(4);

        increment_write_open(&mut table, key).unwrap();
        assert_eq!(
            increment_credential_read(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert_eq!(table.active_refs, 0);
        assert_eq!(table.credential_read_refs, 0);
        assert!(finish_release(release_write_open_in(&mut table, key)));
        assert!(table.entries.is_empty());

        increment_credential_read(&mut table, key).unwrap();
        let counts = table.entries.get(&key).unwrap();
        assert_eq!(counts.active, 1);
        assert_eq!(counts.credential_reads, 1);
        assert_eq!(table.credential_read_refs, 1);
        assert_eq!(
            increment_write_open(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_credential_read_in(&mut table, key)));
        assert!(table.entries.is_empty());
        assert_eq!(table.active_refs, 0);
        assert_eq!(table.credential_read_refs, 0);
    }

    #[test]
    fn identity_and_reference_limits_roll_back_without_partial_counts() {
        let first = test_key(3);
        let second = test_key(4);
        let rejected = test_key(5);
        let mut table = test_table(2);

        increment_active(&mut table, first).unwrap();
        increment_write_open(&mut table, second).unwrap();
        assert_eq!(table.entries.len(), 2);
        assert_eq!(table.active_refs, 1);
        assert_eq!(table.write_open_refs, 1);
        assert_eq!(
            increment_credential_write(&mut table, rejected, false),
            Err(LinuxError::ENFILE.into())
        );
        assert_eq!(table.entries.len(), 2);
        assert_eq!(table.active_refs, 1);
        assert_eq!(table.write_open_refs, 1);
        assert_eq!(table.credential_write_refs, 0);

        let active_before = table.entries.get(&first).unwrap().active;
        table.active_refs = MAX_ACTIVE_EXECUTABLE_REFS;
        assert_eq!(increment_active(&mut table, first), Err(AxError::NoMemory));
        assert_eq!(table.entries.get(&first).unwrap().active, active_before);
        assert_eq!(table.active_refs, MAX_ACTIVE_EXECUTABLE_REFS);
    }

    #[test]
    fn finish_then_active_release_retires_the_exact_identity() {
        let key = test_key(6);
        let mut table = test_table(4);

        increment_credential_read(&mut table, key).unwrap();
        assert!(finish_credential_read_in(&mut table, key));
        let counts = table.entries.get(&key).unwrap();
        assert_eq!(counts.active, 1);
        assert_eq!(counts.credential_reads, 0);
        assert_eq!(table.active_refs, 1);
        assert_eq!(table.credential_read_refs, 0);

        let outcome = release_active_in(&mut table, key);
        assert!(outcome.released);
        assert!(outcome.retired.is_some());
        drop(outcome.retired);
        assert!(table.entries.is_empty());
        assert_eq!(table.active_refs, 0);
    }

    #[test]
    fn credential_metadata_write_and_exec_sampling_exclude_each_other() {
        let key = test_key(7);
        let mut table = test_table(4);

        increment_credential_write(&mut table, key, false).unwrap();
        assert_eq!(table.entries.get(&key).unwrap().credential_writes, 1);
        assert_eq!(table.credential_write_refs, 1);
        assert_eq!(
            increment_credential_write(&mut table, key, false),
            Err(LinuxError::ETXTBSY.into())
        );
        assert_eq!(
            increment_credential_read(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_credential_write_in(&mut table, key)));
        assert!(table.entries.is_empty());

        increment_credential_read(&mut table, key).unwrap();
        assert_eq!(
            increment_credential_write(&mut table, key, false),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_credential_read_in(&mut table, key)));
        assert!(table.entries.is_empty());
    }

    #[test]
    fn file_capability_writer_excludes_old_and_new_content_writers() {
        let key = test_key(8);
        let mut table = test_table(4);

        increment_write_open(&mut table, key).unwrap();
        assert_eq!(
            increment_credential_write(&mut table, key, true),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_write_open_in(&mut table, key)));

        increment_writable_mapping(&mut table, key).unwrap();
        assert_eq!(
            increment_credential_write(&mut table, key, true),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_writable_mapping_in(&mut table, key)));

        increment_credential_write(&mut table, key, true).unwrap();
        assert_eq!(
            increment_write_open(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert_eq!(
            increment_writable_mapping(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_credential_write_in(&mut table, key)));
        assert!(table.entries.is_empty());
    }

    #[test]
    fn writable_mapping_and_exec_sampling_exclude_each_other_and_refund() {
        let key = test_key(9);
        let mut table = test_table(4);

        increment_writable_mapping(&mut table, key).unwrap();
        assert_eq!(table.writable_mapping_refs, 1);
        assert_eq!(
            increment_credential_read(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_writable_mapping_in(&mut table, key)));
        assert_eq!(table.writable_mapping_refs, 0);

        increment_credential_read(&mut table, key).unwrap();
        assert_eq!(
            increment_writable_mapping(&mut table, key),
            Err(LinuxError::ETXTBSY.into())
        );
        assert!(finish_release(release_credential_read_in(&mut table, key)));
        assert!(table.entries.is_empty());
    }
}
