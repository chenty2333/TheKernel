use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use axerrno::{AxError, AxResult};
use hashbrown::{HashMap, HashSet};
use lazy_static::lazy_static;
use linux_raw_sys::general::{
    DN_ACCESS, DN_ATTRIB, DN_CREATE, DN_DELETE, DN_MODIFY, DN_MULTISHOT, DN_RENAME, IN_ACCESS,
    IN_ATTRIB, IN_CREATE, IN_DELETE, IN_MODIFY, POLL_MSG,
};
use spin::Mutex;

use super::{
    AsyncIoOwner, AsyncIoState, FdTableId, FileDescription, FileDescriptionId, inotify::WatchKey,
    send_sigio,
};

const MAX_DNOTIFY_MARKS: usize = 16_384;
const DNOTIFY_TABLE_CLEANUP_BUDGET: usize = 64;
const DNOTIFY_EVENT_MASK: u32 =
    DN_ACCESS | DN_MODIFY | DN_ATTRIB | DN_CREATE | DN_DELETE | DN_RENAME;
const DNOTIFY_ALLOWED_MASK: u32 = DNOTIFY_EVENT_MASK | DN_MULTISHOT;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct OwnerKey {
    table: FdTableId,
    description: FileDescriptionId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct MarkId(u64);

struct Mark {
    owner: OwnerKey,
    watch: WatchKey,
    fd: i32,
    mask: u32,
    description: Weak<FileDescription>,
}

struct Delivery {
    fd: i32,
    description: Arc<FileDescription>,
    state: AsyncIoState,
}

/// Preallocated ownership transferred by the final `FdTable` drop.  The drop
/// path only links this node into an intrusive stack; registry mutation and
/// mark destruction stay in the dedicated policy worker.
pub(crate) struct TableCleanupWork {
    next: AtomicPtr<TableCleanupWork>,
    table: FdTableId,
}

static TABLE_CLEANUP_INCOMING: AtomicPtr<TableCleanupWork> = AtomicPtr::new(ptr::null_mut());
static TABLE_CLEANUP_PENDING: AtomicPtr<TableCleanupWork> = AtomicPtr::new(ptr::null_mut());
static TABLE_CLEANUP_DRAINING: AtomicBool = AtomicBool::new(false);

struct TableCleanupDrainGuard;

impl TableCleanupDrainGuard {
    fn try_enter() -> Option<Self> {
        TABLE_CLEANUP_DRAINING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(Self)
    }
}

impl Drop for TableCleanupDrainGuard {
    fn drop(&mut self) {
        TABLE_CLEANUP_DRAINING.store(false, Ordering::Release);
    }
}

impl TableCleanupWork {
    pub(crate) fn try_new(table: FdTableId) -> AxResult<Box<Self>> {
        Box::try_new(Self {
            next: AtomicPtr::new(ptr::null_mut()),
            table,
        })
        .map_err(|_| AxError::NoMemory)
    }
}

pub(crate) struct DetachedMark {
    _mark: Mark,
    _empty_watch_set: Option<HashSet<MarkId>>,
    _empty_table_set: Option<HashSet<MarkId>>,
}

struct Registry {
    next_mark_id: u64,
    limit: usize,
    marks: HashMap<MarkId, Mark>,
    by_owner: HashMap<OwnerKey, MarkId>,
    by_watch: HashMap<WatchKey, HashSet<MarkId>>,
    by_table: HashMap<FdTableId, HashSet<MarkId>>,
}

impl Registry {
    fn new() -> Self {
        Self::with_limit(MAX_DNOTIFY_MARKS)
    }

    fn with_limit(limit: usize) -> Self {
        Self {
            next_mark_id: 1,
            limit,
            marks: HashMap::new(),
            by_owner: HashMap::new(),
            by_watch: HashMap::new(),
            by_table: HashMap::new(),
        }
    }

    fn allocate_mark_id(&mut self) -> AxResult<MarkId> {
        let id = MarkId(self.next_mark_id);
        self.next_mark_id = self.next_mark_id.checked_add(1).ok_or(AxError::NoMemory)?;
        Ok(id)
    }

    fn reserve_new_mark(&mut self, table: FdTableId, watch: WatchKey) -> AxResult<PreparedSets> {
        if self.marks.len() >= self.limit {
            return Err(AxError::NoMemory);
        }

        self.marks.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        self.by_owner
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.by_watch
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;
        self.by_table
            .try_reserve(1)
            .map_err(|_| AxError::NoMemory)?;

        let new_watch_set = if let Some(ids) = self.by_watch.get_mut(&watch) {
            ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            None
        } else {
            let mut ids = HashSet::new();
            ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            Some(ids)
        };
        let new_table_set = if let Some(ids) = self.by_table.get_mut(&table) {
            ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            None
        } else {
            let mut ids = HashSet::new();
            ids.try_reserve(1).map_err(|_| AxError::NoMemory)?;
            Some(ids)
        };

        Ok(PreparedSets {
            new_watch_set,
            new_table_set,
        })
    }

    fn upsert(
        &mut self,
        table: FdTableId,
        fd: i32,
        description: &Arc<FileDescription>,
        watch: WatchKey,
        mask: u32,
    ) -> AxResult<MarkId> {
        let owner = OwnerKey {
            table,
            description: description.id(),
        };
        if let Some(id) = self.by_owner.get(&owner).copied() {
            let mark = self.marks.get_mut(&id).ok_or(AxError::Io)?;
            if mark.watch != watch {
                return Err(AxError::Io);
            }
            mark.fd = fd;
            mark.mask |= mask;
            mark.description = Arc::downgrade(description);
            return Ok(id);
        }

        let prepared = self.reserve_new_mark(table, watch)?;
        let id = self.allocate_mark_id()?;

        if let Some(mut ids) = prepared.new_watch_set {
            ids.insert(id);
            self.by_watch.insert(watch, ids);
        } else {
            self.by_watch.get_mut(&watch).ok_or(AxError::Io)?.insert(id);
        }
        if let Some(mut ids) = prepared.new_table_set {
            ids.insert(id);
            self.by_table.insert(table, ids);
        } else {
            self.by_table.get_mut(&table).ok_or(AxError::Io)?.insert(id);
        }

        self.marks.insert(
            id,
            Mark {
                owner,
                watch,
                fd,
                mask,
                description: Arc::downgrade(description),
            },
        );
        self.by_owner.insert(owner, id);
        Ok(id)
    }

    fn detach_mark(&mut self, id: MarkId) -> Option<DetachedMark> {
        let mark = self.marks.remove(&id)?;

        if self.by_owner.get(&mark.owner) == Some(&id) {
            self.by_owner.remove(&mark.owner);
        }

        let remove_watch_set = self.by_watch.get_mut(&mark.watch).is_some_and(|ids| {
            ids.remove(&id);
            ids.is_empty()
        });
        let empty_watch_set = remove_watch_set
            .then(|| self.by_watch.remove(&mark.watch))
            .flatten();

        let remove_table_set = self.by_table.get_mut(&mark.owner.table).is_some_and(|ids| {
            ids.remove(&id);
            ids.is_empty()
        });
        let empty_table_set = remove_table_set
            .then(|| self.by_table.remove(&mark.owner.table))
            .flatten();
        Some(DetachedMark {
            _mark: mark,
            _empty_watch_set: empty_watch_set,
            _empty_table_set: empty_table_set,
        })
    }

    fn remove_owner(
        &mut self,
        table: FdTableId,
        description: FileDescriptionId,
    ) -> Option<DetachedMark> {
        let owner = OwnerKey { table, description };
        let id = self.by_owner.get(&owner).copied()?;
        self.detach_mark(id)
    }

    fn detach_one_from_table(&mut self, table: FdTableId) -> Option<DetachedMark> {
        let id = self.by_table.get(&table)?.iter().next().copied()?;
        self.detach_mark(id)
    }

    fn table_has_marks(&self, table: FdTableId) -> bool {
        self.by_table.get(&table).is_some_and(|ids| !ids.is_empty())
    }

    fn watch_mark_count(&self, watch: WatchKey) -> usize {
        self.by_watch.get(&watch).map_or(0, HashSet::len)
    }

    fn collect_deliveries_admitted(
        &mut self,
        watch: WatchKey,
        event: u32,
        collected: &mut CollectedDeliveries,
    ) {
        let Some(ids) = self.by_watch.get(&watch) else {
            return;
        };
        debug_assert!(collected.can_hold(ids.len()));

        for id in ids.iter().copied() {
            let Some(mark) = self.marks.get(&id) else {
                collected.removals.push(id);
                continue;
            };
            let Some(description) = mark.description.upgrade() else {
                collected.removals.push(id);
                continue;
            };
            if mark.mask & event == 0 {
                continue;
            }
            let state = description.async_io_state();
            collected.deliveries.push(Delivery {
                fd: mark.fd,
                description,
                state,
            });
            if mark.mask & DN_MULTISHOT == 0 {
                collected.removals.push(id);
            }
        }

        for id in collected.removals.drain(..) {
            if let Some(mark) = self.detach_mark(id) {
                collected.detached.push(mark);
            } else if let Some(ids) = self.by_watch.get_mut(&watch) {
                ids.remove(&id);
            }
        }
        collected.orphaned_watch_set = self
            .by_watch
            .get(&watch)
            .is_some_and(HashSet::is_empty)
            .then(|| self.by_watch.remove(&watch))
            .flatten();
    }

    #[cfg(test)]
    fn collect_deliveries(&mut self, watch: WatchKey, event: u32) -> AxResult<CollectedDeliveries> {
        let mut collected = CollectedDeliveries::default();
        collected.try_admit(self.watch_mark_count(watch))?;
        self.collect_deliveries_admitted(watch, event, &mut collected);
        Ok(collected)
    }
}

#[derive(Default)]
struct CollectedDeliveries {
    deliveries: Vec<Delivery>,
    detached: Vec<DetachedMark>,
    removals: Vec<MarkId>,
    orphaned_watch_set: Option<HashSet<MarkId>>,
}

impl CollectedDeliveries {
    fn can_hold(&self, required: usize) -> bool {
        self.deliveries.capacity() >= required
            && self.detached.capacity() >= required
            && self.removals.capacity() >= required
    }

    fn try_admit(&mut self, required: usize) -> AxResult<()> {
        self.deliveries
            .try_reserve_exact(required.saturating_sub(self.deliveries.len()))
            .map_err(|_| AxError::NoMemory)?;
        self.detached
            .try_reserve_exact(required.saturating_sub(self.detached.len()))
            .map_err(|_| AxError::NoMemory)?;
        self.removals
            .try_reserve_exact(required.saturating_sub(self.removals.len()))
            .map_err(|_| AxError::NoMemory)
    }
}

struct PreparedSets {
    new_watch_set: Option<HashSet<MarkId>>,
    new_table_set: Option<HashSet<MarkId>>,
}

lazy_static! {
    static ref REGISTRY: Mutex<Registry> = Mutex::new(Registry::new());
}

pub(crate) fn is_remove_mask(mask: u32) -> bool {
    mask & !DN_MULTISHOT == 0
}

pub(crate) fn converted_mask(mask: u32) -> u32 {
    mask & DNOTIFY_ALLOWED_MASK
}

pub(crate) const fn mask_from_fcntl_arg(arg: usize) -> u32 {
    // Linux fcntl_dirnotify takes unsigned int: the syscall's unsigned long
    // argument is truncated before convert_arg filters unknown low bits.
    arg as u32
}

/// Registers or augments a dnotify mark for one Linux fd table and open file
/// description. The caller has already validated the directory and computed
/// its stable watch key without holding the fd table lock. It must then hold a
/// short fd table read lock while calling this so close/replace can linearize
/// with the registry update.
pub(crate) fn set_watch(
    table: FdTableId,
    fd: i32,
    description: &Arc<FileDescription>,
    watch: WatchKey,
    mask: u32,
) -> AxResult<()> {
    let mut registry = REGISTRY.lock();
    registry.upsert(table, fd, description, watch, mask)?;
    // Linux F_NOTIFY only installs the current TGID when no F_SETOWN owner is
    // present. Publish it before releasing the registry lock so an event can
    // never claim the new mark with a transient empty owner.
    description.ensure_async_io_owner(AsyncIoOwner::current_process());
    Ok(())
}

/// Removes the mark owned by an exact fd-table/open-file-description pair.
/// Close and replace callers must invoke this while holding the table write
/// lock; the registry never acquires a table lock in the opposite direction.
pub(crate) fn detach_watch(
    table: FdTableId,
    description: FileDescriptionId,
) -> Option<DetachedMark> {
    REGISTRY.lock().remove_owner(table, description)
}

/// Publishes preallocated table-teardown ownership from `FdTable::drop`.
/// This path is lock-free and allocation-free; the policy worker owns all
/// registry mutation and destruction.
pub(crate) fn publish_table_cleanup(work: Box<TableCleanupWork>) {
    let work = Box::into_raw(work);
    let mut head = TABLE_CLEANUP_INCOMING.load(Ordering::Acquire);
    loop {
        // SAFETY: `work` remains exclusively owned by this producer until the
        // successful publication transfers it to the intrusive stack.
        unsafe { (*work).next.store(head, Ordering::Relaxed) };
        match TABLE_CLEANUP_INCOMING.compare_exchange_weak(
            head,
            work,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(observed) => head = observed,
        }
    }
}

fn refill_pending_table_cleanup() {
    if !TABLE_CLEANUP_PENDING.load(Ordering::Relaxed).is_null() {
        return;
    }

    // Detach one finite producer batch, then reverse the Treiber stack. New
    // producers publish into INCOMING and cannot overtake this FIFO snapshot.
    let mut current = TABLE_CLEANUP_INCOMING.swap(ptr::null_mut(), Ordering::AcqRel);
    let mut reversed = ptr::null_mut();
    while !current.is_null() {
        // SAFETY: the swap gave this single drainer exclusive ownership of the
        // detached batch.
        let next = unsafe { (*current).next.load(Ordering::Relaxed) };
        unsafe { (*current).next.store(reversed, Ordering::Relaxed) };
        reversed = current;
        current = next;
    }
    TABLE_CLEANUP_PENDING.store(reversed, Ordering::Relaxed);
}

fn pop_table_cleanup() -> Option<Box<TableCleanupWork>> {
    if TABLE_CLEANUP_PENDING.load(Ordering::Relaxed).is_null() {
        refill_pending_table_cleanup();
    }
    let head = TABLE_CLEANUP_PENDING.load(Ordering::Relaxed);
    if head.is_null() {
        return None;
    }
    // SAFETY: TABLE_CLEANUP_DRAINING admits one consumer, and producers never
    // access the private PENDING list.
    let next = unsafe { (*head).next.load(Ordering::Relaxed) };
    TABLE_CLEANUP_PENDING.store(next, Ordering::Relaxed);
    unsafe { (*head).next.store(ptr::null_mut(), Ordering::Relaxed) };
    Some(unsafe { Box::from_raw(head) })
}

pub(crate) fn has_deferred_table_cleanup_work() -> bool {
    !TABLE_CLEANUP_INCOMING.load(Ordering::Acquire).is_null()
        || !TABLE_CLEANUP_PENDING.load(Ordering::Acquire).is_null()
}

/// Removes at most one fixed-size batch of marks. Empty registry sets, weak
/// descriptions, and the work allocation itself are all dropped outside the
/// global registry lock.
pub(crate) fn drain_table_cleanup_work() {
    let Some(_guard) = TableCleanupDrainGuard::try_enter() else {
        return;
    };
    let Some(work) = pop_table_cleanup() else {
        return;
    };
    for _ in 0..DNOTIFY_TABLE_CLEANUP_BUDGET {
        let detached = REGISTRY.lock().detach_one_from_table(work.table);
        let Some(detached) = detached else {
            break;
        };
        drop(detached);
    }
    let has_more = REGISTRY.lock().table_has_marks(work.table);
    if has_more {
        publish_table_cleanup(work);
    }
}

fn deliver(delivery: Delivery) {
    let Delivery {
        fd,
        description,
        state,
    } = delivery;
    send_sigio(&state, fd, POLL_MSG);
    drop(description);
}

/// Emits one DN_* event. Delivery is deliberately performed after releasing
/// the registry lock, and owner/signal state is read from the live OFD.
pub(crate) fn emit(watch: WatchKey, event: u32) -> AxResult<()> {
    let mut collected = CollectedDeliveries::default();
    loop {
        let required = REGISTRY.lock().watch_mark_count(watch);
        collected.try_admit(required)?;
        let complete = {
            let mut registry = REGISTRY.lock();
            let required = registry.watch_mark_count(watch);
            if !collected.can_hold(required) {
                false
            } else {
                registry.collect_deliveries_admitted(watch, event, &mut collected);
                true
            }
        };
        if complete {
            break;
        }
    }
    let CollectedDeliveries {
        deliveries,
        detached,
        removals,
        orphaned_watch_set,
    } = collected;
    drop(detached);
    drop(removals);
    drop(orphaned_watch_set);
    for delivery in deliveries {
        deliver(delivery);
    }
    Ok(())
}

/// Maps ordinary inotify-style notifications to their dnotify counterparts.
/// IN_MOVED_FROM/IN_MOVED_TO are intentionally excluded: rename dispatch must
/// decide atomically between same-parent DN_RENAME and cross-parent
/// DN_DELETE/DN_CREATE before a one-shot mark can be consumed.
pub(crate) fn emit_inotify(watch: WatchKey, mask: u32) -> AxResult<()> {
    let mut event = 0;
    if mask & IN_ACCESS != 0 {
        event |= DN_ACCESS;
    }
    if mask & IN_MODIFY != 0 {
        event |= DN_MODIFY;
    }
    if mask & IN_ATTRIB != 0 {
        event |= DN_ATTRIB;
    }
    if mask & IN_CREATE != 0 {
        event |= DN_CREATE;
    }
    if mask & IN_DELETE != 0 {
        event |= DN_DELETE;
    }
    if event == 0 {
        Ok(())
    } else {
        emit(watch, event)
    }
}

fn emit_rename_with(
    old_parent: WatchKey,
    new_parent: WatchKey,
    mut emit_one: impl FnMut(WatchKey, u32) -> AxResult<()>,
) -> AxResult<()> {
    let mut first_error = None;
    for (watch, event) in [
        (old_parent, DN_RENAME),
        (old_parent, DN_DELETE),
        (new_parent, DN_CREATE),
    ] {
        if let Err(error) = emit_one(watch, event)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub(crate) fn emit_rename(old_parent: WatchKey, new_parent: WatchKey) -> AxResult<()> {
    emit_rename_with(old_parent, new_parent, emit)
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, sync::Arc};
    use core::task::Context;

    use axpoll::{IoEvents, Pollable};
    use starry_signal::Signo;

    use super::*;
    use crate::file::{FdTable, FileLike, Kstat};

    struct TestFile;

    impl Pollable for TestFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileLike for TestFile {
        fn stat(&self) -> AxResult<Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, str>> {
            Ok(Cow::Borrowed("dnotify-test"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    fn description() -> Arc<FileDescription> {
        FileDescription::new(Arc::new(TestFile)).unwrap()
    }

    fn table_id() -> FdTableId {
        FdTable::new().unwrap().id()
    }

    const WATCH_A: WatchKey = WatchKey { dev: 1, ino: 10 };
    const WATCH_B: WatchKey = WatchKey { dev: 1, ino: 11 };

    #[test]
    fn repeated_registration_accumulates_mask_and_updates_signal_fd() {
        let mut registry = Registry::new();
        let table = table_id();
        let description = description();
        let first = registry
            .upsert(table, 3, &description, WATCH_A, DN_ACCESS | DN_MULTISHOT)
            .unwrap();
        let second = registry
            .upsert(table, 7, &description, WATCH_A, DN_ATTRIB)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(registry.marks.len(), 1);
        let mark = registry.marks.get(&first).unwrap();
        assert_eq!(mark.fd, 7);
        assert_eq!(
            mark.mask & (DN_ACCESS | DN_ATTRIB | DN_MULTISHOT),
            DN_ACCESS | DN_ATTRIB | DN_MULTISHOT
        );
    }

    #[test]
    fn owner_identity_includes_table_and_open_file_description() {
        let mut registry = Registry::new();
        let table_a = table_id();
        let table_b = table_id();
        let description_a = description();
        let description_b = description();

        registry
            .upsert(table_a, 3, &description_a, WATCH_A, DN_ATTRIB)
            .unwrap();
        registry
            .upsert(table_b, 3, &description_a, WATCH_A, DN_ATTRIB)
            .unwrap();
        registry
            .upsert(table_a, 3, &description_b, WATCH_A, DN_ATTRIB)
            .unwrap();

        assert!(registry.remove_owner(table_a, description_a.id()).is_some());
        assert_eq!(registry.marks.len(), 2);
        assert!(registry.by_owner.contains_key(&OwnerKey {
            table: table_b,
            description: description_a.id(),
        }));
        assert!(registry.by_owner.contains_key(&OwnerKey {
            table: table_a,
            description: description_b.id(),
        }));
    }

    #[test]
    fn oneshot_is_removed_but_multishot_and_other_watch_survive() {
        let mut registry = Registry::new();
        let table = table_id();
        let oneshot = description();
        let multishot = description();
        let other = description();

        registry
            .upsert(table, 3, &oneshot, WATCH_A, DN_ATTRIB)
            .unwrap();
        registry
            .upsert(table, 4, &multishot, WATCH_A, DN_ATTRIB | DN_MULTISHOT)
            .unwrap();
        registry
            .upsert(table, 5, &other, WATCH_B, DN_ATTRIB)
            .unwrap();

        let deliveries = registry.collect_deliveries(WATCH_A, DN_ATTRIB).unwrap();
        assert_eq!(deliveries.deliveries.len(), 2);
        assert!(!registry.by_owner.contains_key(&OwnerKey {
            table,
            description: oneshot.id(),
        }));
        assert!(registry.by_owner.contains_key(&OwnerKey {
            table,
            description: multishot.id(),
        }));
        assert!(registry.by_owner.contains_key(&OwnerKey {
            table,
            description: other.id(),
        }));
    }

    #[test]
    fn stale_mark_id_cannot_remove_a_new_registration() {
        let mut registry = Registry::new();
        let table = table_id();
        let description = description();
        let old = registry
            .upsert(table, 3, &description, WATCH_A, DN_ATTRIB)
            .unwrap();
        assert!(registry.detach_mark(old).is_some());
        let new = registry
            .upsert(table, 3, &description, WATCH_A, DN_ATTRIB)
            .unwrap();

        assert_ne!(old, new);
        assert!(registry.detach_mark(old).is_none());
        assert_eq!(
            registry.by_owner.get(&OwnerKey {
                table,
                description: description.id(),
            }),
            Some(&new)
        );
    }

    #[test]
    fn weak_description_is_pruned_without_delivery() {
        let mut registry = Registry::new();
        let table = table_id();
        let description = description();
        registry
            .upsert(table, 3, &description, WATCH_A, DN_ATTRIB | DN_MULTISHOT)
            .unwrap();
        drop(description);

        assert!(
            registry
                .collect_deliveries(WATCH_A, DN_ATTRIB)
                .unwrap()
                .deliveries
                .is_empty()
        );
        assert!(registry.marks.is_empty());
    }

    #[test]
    fn delivery_claim_holds_description_and_snapshots_signal_state() {
        let mut registry = Registry::new();
        let table = table_id();
        let description = description();
        registry
            .upsert(table, 3, &description, WATCH_A, DN_ATTRIB | DN_MULTISHOT)
            .unwrap();
        description.set_async_io_signal(Signo::SIGUSR1 as u8);

        let delivery = registry
            .collect_deliveries(WATCH_A, DN_ATTRIB)
            .unwrap()
            .deliveries
            .pop()
            .unwrap();
        drop(description);
        assert_eq!(delivery.state.signal, Signo::SIGUSR1 as u8);
        assert_eq!(Arc::strong_count(&delivery.description), 1);
    }

    #[test]
    fn table_removal_is_indexed_and_exact() {
        let mut registry = Registry::new();
        let table_a = table_id();
        let table_b = table_id();
        let description_a = description();
        let description_b = description();
        registry
            .upsert(table_a, 3, &description_a, WATCH_A, DN_ATTRIB)
            .unwrap();
        registry
            .upsert(table_b, 3, &description_b, WATCH_A, DN_ATTRIB)
            .unwrap();

        while registry.detach_one_from_table(table_a).is_some() {}
        assert_eq!(registry.marks.len(), 1);
        assert!(registry.by_table.contains_key(&table_b));
        assert!(!registry.by_table.contains_key(&table_a));
    }

    #[test]
    fn mark_limit_fails_without_partial_registration() {
        let mut registry = Registry::with_limit(1);
        let table = table_id();
        let first = description();
        let second = description();
        registry
            .upsert(table, 3, &first, WATCH_A, DN_ATTRIB)
            .unwrap();

        assert_eq!(
            registry.upsert(table, 4, &second, WATCH_B, DN_ATTRIB),
            Err(AxError::NoMemory)
        );
        assert_eq!(registry.marks.len(), 1);
        assert_eq!(registry.by_owner.len(), 1);
        assert_eq!(registry.by_watch.len(), 1);
    }

    #[test]
    fn zero_or_multishot_only_mask_is_an_exact_withdrawal() {
        assert!(is_remove_mask(0));
        assert!(is_remove_mask(DN_MULTISHOT));
        assert!(!is_remove_mask(DN_ATTRIB));
        assert!(!is_remove_mask(DN_ATTRIB | DN_MULTISHOT));
    }

    #[test]
    fn unknown_low_bits_are_ignored_but_do_not_withdraw() {
        const UNKNOWN: u32 = 1 << 6;
        assert!(!is_remove_mask(UNKNOWN));
        assert_eq!(converted_mask(UNKNOWN), 0);
        assert_eq!(converted_mask(UNKNOWN | DN_ATTRIB), DN_ATTRIB);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn f_notify_truncates_high_argument_bits_like_unsigned_int() {
        let arg = (1usize << 32) | DN_ATTRIB as usize;
        assert_eq!(mask_from_fcntl_arg(arg), DN_ATTRIB);
    }

    #[test]
    fn rename_always_emits_linux_three_event_sequence() {
        let mut events = Vec::new();
        emit_rename_with(WATCH_A, WATCH_A, |watch, event| {
            events.push((watch, event));
            Ok(())
        })
        .unwrap();

        assert_eq!(
            events,
            [
                (WATCH_A, DN_RENAME),
                (WATCH_A, DN_DELETE),
                (WATCH_A, DN_CREATE),
            ]
        );
    }

    #[test]
    fn rename_attempts_all_events_and_returns_first_error() {
        let mut calls = 0;
        let result = emit_rename_with(WATCH_A, WATCH_B, |_, _| {
            calls += 1;
            if calls == 1 { Err(AxError::Io) } else { Ok(()) }
        });

        assert_eq!(result, Err(AxError::Io));
        assert_eq!(calls, 3);
    }
}
