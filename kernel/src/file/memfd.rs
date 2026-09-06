use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::Location;
use axsync::spin::SpinNoIrq;
#[cfg(not(test))]
use axtask::WaitQueue;
use linux_raw_sys::general::{
    F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, MFD_ALLOW_SEALING, MFD_CLOEXEC,
};

#[cfg(test)]
extern crate std;

#[cfg(test)]
use std::sync::{Condvar, Mutex as StdMutex};

pub(crate) const MEMFD_SUPPORTED_CREATE_FLAGS: u32 = MFD_CLOEXEC | MFD_ALLOW_SEALING;
pub(crate) const MEMFD_SUPPORTED_SEALS: u32 =
    F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;

#[derive(Debug)]
struct MemfdSealState {
    seals: u32,
    writable_shared_mappings: usize,
    active_writes: usize,
    resize_active: bool,
    seal_update_active: bool,
    pending_seals: u32,
}

pub(crate) struct MemfdState {
    state: SpinNoIrq<MemfdSealState>,
    changed: MemfdStateChanged,
}

#[cfg(not(test))]
struct MemfdStateChanged {
    waiters: WaitQueue,
}

#[cfg(not(test))]
impl MemfdStateChanged {
    const fn new() -> Self {
        Self {
            waiters: WaitQueue::new(),
        }
    }

    fn wait_until(&self, condition: impl FnMut() -> bool) -> AxResult<()> {
        self.waiters.wait_until(condition).map_err(Into::into)
    }

    fn notify_all(&self) {
        self.waiters.notify_all(false);
    }
}

#[cfg(test)]
struct MemfdStateChanged {
    generation: StdMutex<u64>,
    changed: Condvar,
    fail_next_wait: AtomicBool,
}

#[cfg(test)]
impl MemfdStateChanged {
    const fn new() -> Self {
        Self {
            generation: StdMutex::new(0),
            changed: Condvar::new(),
            fail_next_wait: AtomicBool::new(false),
        }
    }

    fn wait_until(&self, mut condition: impl FnMut() -> bool) -> AxResult<()> {
        if self.fail_next_wait.swap(false, Ordering::AcqRel) {
            return Err(AxError::ResourceBusy);
        }
        let mut generation = self.generation.lock().unwrap();
        while !condition() {
            generation = self.changed.wait(generation).unwrap();
        }
        Ok(())
    }

    fn notify_all(&self) {
        let mut generation = self.generation.lock().unwrap();
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    fn fail_next_wait(&self) {
        self.fail_next_wait.store(true, Ordering::Release);
    }
}

/// Owns an in-progress seal publication until it is committed or rolled back.
///
/// A wait failure after `seal_update_active` becomes visible must not leave the
/// memfd permanently blocked or publish seals before existing mutations drain.
struct PendingSealUpdate<'state> {
    owner: &'state MemfdState,
    seals: u32,
    active: bool,
}

impl<'state> PendingSealUpdate<'state> {
    fn new(owner: &'state MemfdState, seals: u32) -> Self {
        Self {
            owner,
            seals,
            active: true,
        }
    }

    fn commit(mut self) -> AxResult<u32> {
        let mut state = self.owner.state.lock();
        if !state.seal_update_active || state.pending_seals != self.seals {
            return Err(AxError::BadState);
        }
        state.seals |= self.seals;
        state.pending_seals = 0;
        state.seal_update_active = false;
        let result = state.seals;
        self.active = false;
        drop(state);
        self.owner.changed.notify_all();
        Ok(result)
    }
}

impl Drop for PendingSealUpdate<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self.owner.state.lock();
        if !state.seal_update_active || state.pending_seals != self.seals {
            // Preserve exclusion if ownership state was corrupted; clearing a
            // different owner's publication would be less safe than blocking.
            return;
        }
        state.pending_seals = 0;
        state.seal_update_active = false;
        drop(state);
        self.owner.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemfdMutationKind {
    Write,
    Resize,
}

/// Owns one memfd mutation admission until the associated backend operation
/// has completed or failed. No spin guard is retained across the operation.
#[must_use = "a memfd mutation reservation must cover its backend operation"]
pub(crate) struct MemfdMutationGuard {
    state: Option<Arc<MemfdState>>,
    kind: Option<MemfdMutationKind>,
}

impl MemfdMutationGuard {
    const fn untracked() -> Self {
        Self {
            state: None,
            kind: None,
        }
    }

    fn tracked(state: Arc<MemfdState>, kind: MemfdMutationKind) -> Self {
        Self {
            state: Some(state),
            kind: Some(kind),
        }
    }

    /// Admits the exact write range after placement is known.
    ///
    /// This method is intentionally nonblocking: callers may invoke it inside
    /// an inode append transaction. `file_len` must be the length observed in
    /// that same placement domain. The reservation itself must have been
    /// acquired before taking any axfs append/backend lock.
    pub(crate) fn admit_write(
        &self,
        loc: &Location,
        file_len: u64,
        offset: u64,
        len: usize,
    ) -> AxResult<()> {
        if len == 0 {
            return Ok(());
        }
        let Some(reserved) = self.state.as_ref() else {
            return if state_for_location(loc).is_none() {
                Ok(())
            } else {
                Err(AxError::BadState)
            };
        };
        if self.kind != Some(MemfdMutationKind::Write) {
            return Err(AxError::BadState);
        }
        let Some(actual) = state_for_location(loc) else {
            return Err(AxError::BadState);
        };
        if !Arc::ptr_eq(reserved, &actual) {
            return Err(AxError::BadState);
        }
        reserved.admit_write_range(file_len, offset, len)
    }
}

impl Drop for MemfdMutationGuard {
    fn drop(&mut self) {
        let (Some(state), Some(kind)) = (self.state.as_ref(), self.kind.take()) else {
            return;
        };
        let mut seal_state = state.state.lock();
        match kind {
            MemfdMutationKind::Write => {
                assert!(
                    seal_state.active_writes != 0,
                    "memfd write reservation underflow"
                );
                seal_state.active_writes -= 1;
            }
            MemfdMutationKind::Resize => {
                assert!(
                    seal_state.resize_active,
                    "inactive memfd resize reservation"
                );
                seal_state.resize_active = false;
            }
        }
        drop(seal_state);
        state.changed.notify_all();
    }
}

impl MemfdState {
    fn new(allow_sealing: bool) -> Self {
        let initial = if allow_sealing { 0 } else { F_SEAL_SEAL };
        Self {
            state: SpinNoIrq::new(MemfdSealState {
                seals: initial,
                writable_shared_mappings: 0,
                active_writes: 0,
                resize_active: false,
                seal_update_active: false,
                pending_seals: 0,
            }),
            changed: MemfdStateChanged::new(),
        }
    }

    fn seals(&self) -> u32 {
        self.state.lock().seals
    }

    fn add_writable_mapping(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.seals & F_SEAL_WRITE != 0
            || state.seal_update_active && state.pending_seals & F_SEAL_WRITE != 0
        {
            return Err(AxError::OperationNotPermitted);
        }
        state.writable_shared_mappings = state
            .writable_shared_mappings
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        Ok(())
    }

    fn remove_writable_mapping(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.writable_shared_mappings == 0 {
            // Accounting corruption must not make F_SEAL_WRITE observable as
            // safe. Poison the count so future sealing and writable admission
            // both fail closed instead of turning an internal bug into an
            // untracked writable mapping (or a production panic).
            state.writable_shared_mappings = usize::MAX;
            return Err(AxError::BadState);
        }
        state.writable_shared_mappings -= 1;
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    fn add_seals(&self, seals: u32) -> AxResult<u32> {
        loop {
            let mut state = self.state.lock();
            if !state.seal_update_active {
                if state.seals & F_SEAL_SEAL != 0 {
                    return Err(AxError::OperationNotPermitted);
                }
                if seals & F_SEAL_WRITE != 0 && state.writable_shared_mappings != 0 {
                    return Err(AxError::ResourceBusy);
                }
                state.seal_update_active = true;
                state.pending_seals = seals;
                break;
            }
            drop(state);
            self.changed
                .wait_until(|| !self.state.lock().seal_update_active)?;
        }

        let update = PendingSealUpdate::new(self, seals);
        // The pending owner prevents new mutation admission. Existing owned
        // reservations drain without any spin guard being held by this waiter.
        self.changed.wait_until(|| {
            let state = self.state.lock();
            state.active_writes == 0 && !state.resize_active
        })?;

        update.commit()
    }

    fn reserve_write(self: &Arc<Self>) -> AxResult<MemfdMutationGuard> {
        loop {
            let mut state = self.state.lock();
            if !state.seal_update_active && !state.resize_active {
                state.active_writes = state
                    .active_writes
                    .checked_add(1)
                    .ok_or(AxError::NoMemory)?;
                return Ok(MemfdMutationGuard::tracked(
                    self.clone(),
                    MemfdMutationKind::Write,
                ));
            }
            drop(state);
            self.changed.wait_until(|| {
                let state = self.state.lock();
                !state.seal_update_active && !state.resize_active
            })?;
        }
    }

    fn reserve_resize(self: &Arc<Self>) -> AxResult<MemfdMutationGuard> {
        loop {
            let mut state = self.state.lock();
            if !state.seal_update_active && !state.resize_active && state.active_writes == 0 {
                state.resize_active = true;
                return Ok(MemfdMutationGuard::tracked(
                    self.clone(),
                    MemfdMutationKind::Resize,
                ));
            }
            drop(state);
            self.changed.wait_until(|| {
                let state = self.state.lock();
                !state.seal_update_active && !state.resize_active && state.active_writes == 0
            })?;
        }
    }

    fn admit_write_range(&self, old_len: u64, offset: u64, len: usize) -> AxResult<()> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(AxError::InvalidInput)?;
        let state = self.state.lock();
        debug_assert!(state.active_writes != 0 || state.resize_active);
        if state.seals & F_SEAL_WRITE != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        if state.seals & F_SEAL_GROW != 0 && end > old_len {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }

    fn admit_resize(&self, old_len: u64, new_len: u64, writes_content: bool) -> AxResult<()> {
        let state = self.state.lock();
        debug_assert!(state.resize_active);
        if writes_content && state.seals & F_SEAL_WRITE != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        if new_len < old_len && state.seals & F_SEAL_SHRINK != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        if new_len > old_len && state.seals & F_SEAL_GROW != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        Ok(())
    }
}

pub(crate) struct WritableMappingRegistration {
    state: Arc<MemfdState>,
    active: AtomicBool,
}

impl WritableMappingRegistration {
    fn new(state: Arc<MemfdState>) -> Self {
        Self {
            state,
            active: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Atomically excludes `F_SEAL_WRITE` before a shared mapping becomes
    /// writable. The MemfdState lock is the linearization point shared with
    /// `F_ADD_SEALS`, so neither side can pass a stale pair of atomic reads.
    pub(crate) fn set_active(&self, active: bool) -> AxResult<()> {
        if active {
            if self.active.load(Ordering::Acquire) {
                return Ok(());
            }
            self.state.add_writable_mapping()?;
            if self
                .active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                self.state.remove_writable_mapping()?;
            }
            return Ok(());
        }

        if self
            .active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.state.remove_writable_mapping()?;
        }
        Ok(())
    }
}

impl Drop for WritableMappingRegistration {
    fn drop(&mut self) {
        if self.active.load(Ordering::Acquire) {
            // Destructors cannot surface the typed error. The helper poisons
            // the count on underflow, which preserves the sealing exclusion.
            let _ = self.state.remove_writable_mapping();
        }
    }
}

pub(crate) fn install_memfd_state(
    loc: &Location,
    allow_sealing: bool,
) -> AxResult<Arc<MemfdState>> {
    let mut guard = loc.user_data();
    guard.try_get_or_insert_with(|| MemfdState::new(allow_sealing))
}

pub(crate) fn current_seals(loc: &Location) -> Option<u32> {
    let guard = loc.user_data();
    guard.get::<MemfdState>().map(|state| state.seals())
}

pub(crate) fn get_seals(loc: &Location) -> AxResult<u32> {
    current_seals(loc).ok_or(AxError::InvalidInput)
}

pub(crate) fn add_seals(loc: &Location, writable: bool, seals: u32) -> AxResult<u32> {
    if seals & !MEMFD_SUPPORTED_SEALS != 0 {
        return Err(AxError::InvalidInput);
    }

    let state = {
        let guard = loc.user_data();
        guard.get::<MemfdState>().ok_or(AxError::InvalidInput)?
    };

    if !writable {
        return Err(AxError::OperationNotPermitted);
    }

    state.add_seals(seals)
}

fn state_for_location(loc: &Location) -> Option<Arc<MemfdState>> {
    let guard = loc.user_data();
    guard.get::<MemfdState>()
}

/// Reserves one content write in the seal linearization domain.
///
/// This is phase one of write admission and may sleep behind a resize or seal
/// update. It must be called before taking any axfs append/backend lock. Once
/// placement is frozen, call [`MemfdMutationGuard::admit_write`], which is
/// nonblocking, and retain the guard through the complete backend operation.
/// Multiple writes may coexist, but every resize waits for all of them.
pub(crate) fn begin_write(loc: &Location, len: usize) -> AxResult<MemfdMutationGuard> {
    if len == 0 {
        return Ok(MemfdMutationGuard::untracked());
    }
    let Some(state) = state_for_location(loc) else {
        return Ok(MemfdMutationGuard::untracked());
    };
    state.reserve_write()
}

/// Reserves one length-only mutation, excluding writes and other resizes until
/// the returned guard is dropped. This function may sleep and therefore must
/// run before taking any axfs/backend lock.
pub(crate) fn begin_resize(loc: &Location, new_len: u64) -> AxResult<MemfdMutationGuard> {
    let Some(state) = state_for_location(loc) else {
        return Ok(MemfdMutationGuard::untracked());
    };
    let reservation = state.reserve_resize()?;
    let old_len = loc.len()?;
    state.admit_resize(old_len, new_len, false)?;
    Ok(reservation)
}

/// Reserves a compound content-and-length mutation such as zero/collapse/insert
/// fallocate. Both the write seal and the applicable grow/shrink seal are
/// checked against one exclusive reservation. This function may sleep and must
/// run before taking any axfs/backend lock.
pub(crate) fn begin_write_resize(
    loc: &Location,
    offset: u64,
    len: usize,
    new_len: u64,
) -> AxResult<MemfdMutationGuard> {
    let Some(state) = state_for_location(loc) else {
        return Ok(MemfdMutationGuard::untracked());
    };
    let reservation = state.reserve_resize()?;
    let old_len = loc.len()?;
    state.admit_resize(old_len, new_len, true)?;
    if len != 0 {
        state.admit_write_range(old_len, offset, len)?;
    }
    Ok(reservation)
}

pub(crate) fn check_writable_shared_mapping(loc: &Location) -> AxResult<()> {
    if current_seals(loc).is_some_and(|seals| seals & F_SEAL_WRITE != 0) {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

pub(crate) fn new_writable_mapping_registration(
    loc: &Location,
) -> Option<Arc<WritableMappingRegistration>> {
    let state = {
        let guard = loc.user_data();
        guard.get::<MemfdState>()
    }?;
    Some(Arc::new(WritableMappingRegistration::new(state)))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Barrier, RwLock, mpsc},
        time::Duration,
    };

    use axfs_ng_vfs::{Location, Mountpoint, NodePermission, NodeType};

    use super::*;
    use crate::pseudofs::tmp::MemoryFs;

    fn memfd_location(name: &str, len: u64) -> Location {
        let filesystem = MemoryFs::new().unwrap();
        let mount = Mountpoint::new_root(&filesystem);
        let location = mount
            .root_location()
            .create(
                axfs_ng_vfs::FsName::new(name.as_bytes()),
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o600),
            )
            .unwrap();
        location.entry().as_file().unwrap().set_len(len).unwrap();
        install_memfd_state(&location, true).unwrap();
        location
    }

    #[test]
    fn writable_registration_is_idempotent_and_blocks_seal_write() {
        let state = Arc::new(MemfdState::new(true));
        let registration = WritableMappingRegistration::new(state.clone());

        registration.set_active(true).unwrap();
        registration.set_active(true).unwrap();
        assert_eq!(state.state.lock().writable_shared_mappings, 1);
        assert_eq!(state.add_seals(F_SEAL_WRITE), Err(AxError::ResourceBusy));

        registration.set_active(false).unwrap();
        registration.set_active(false).unwrap();
        assert_eq!(state.state.lock().writable_shared_mappings, 0);
        assert_eq!(state.add_seals(F_SEAL_WRITE).unwrap(), F_SEAL_WRITE);
    }

    #[test]
    fn existing_seal_write_rejects_registration_without_accounting_it() {
        let state = Arc::new(MemfdState::new(true));
        state.add_seals(F_SEAL_WRITE).unwrap();
        let registration = WritableMappingRegistration::new(state.clone());

        assert_eq!(
            registration.set_active(true),
            Err(AxError::OperationNotPermitted)
        );
        assert!(!registration.is_active());
        assert_eq!(state.state.lock().writable_shared_mappings, 0);
    }

    #[test]
    fn seal_write_and_writable_admission_have_one_linearization_point() {
        use std::sync::Barrier;

        for _ in 0..64 {
            let state = Arc::new(MemfdState::new(true));
            let registration = Arc::new(WritableMappingRegistration::new(state.clone()));
            let barrier = Arc::new(Barrier::new(3));

            let (activation, sealing) = std::thread::scope(|scope| {
                let activation = scope.spawn({
                    let registration = registration.clone();
                    let barrier = barrier.clone();
                    move || {
                        barrier.wait();
                        registration.set_active(true)
                    }
                });
                let sealing = scope.spawn({
                    let state = state.clone();
                    let barrier = barrier.clone();
                    move || {
                        barrier.wait();
                        state.add_seals(F_SEAL_WRITE)
                    }
                });
                barrier.wait();
                (activation.join().unwrap(), sealing.join().unwrap())
            });

            match (activation, sealing) {
                (Ok(()), Err(AxError::ResourceBusy)) => {
                    registration.set_active(false).unwrap();
                    state.add_seals(F_SEAL_WRITE).unwrap();
                }
                (Err(AxError::OperationNotPermitted), Ok(seals)) => {
                    assert_eq!(seals, F_SEAL_WRITE);
                    assert!(!registration.is_active());
                }
                outcome => panic!("writable admission and F_SEAL_WRITE both escaped: {outcome:?}"),
            }
            assert_eq!(state.state.lock().writable_shared_mappings, 0);
        }
    }

    #[test]
    fn writable_mapping_count_overflow_fails_without_wrapping() {
        let state = Arc::new(MemfdState::new(true));
        state.state.lock().writable_shared_mappings = usize::MAX;

        assert_eq!(state.add_writable_mapping(), Err(AxError::NoMemory));
        assert_eq!(state.state.lock().writable_shared_mappings, usize::MAX);

        // Restore a valid synthetic state so dropping the test value does not
        // leave misleading impossible accounting behind.
        state.state.lock().writable_shared_mappings = 0;
    }

    #[test]
    fn writable_mapping_underflow_poison_is_fail_closed() {
        let state = Arc::new(MemfdState::new(true));

        assert_eq!(state.remove_writable_mapping(), Err(AxError::BadState));
        assert_eq!(state.state.lock().writable_shared_mappings, usize::MAX);
        assert_eq!(state.add_seals(F_SEAL_WRITE), Err(AxError::ResourceBusy));
        assert_eq!(state.add_writable_mapping(), Err(AxError::NoMemory));

        // Restore a valid synthetic state before dropping the test fixture.
        state.state.lock().writable_shared_mappings = 0;
    }

    #[test]
    fn seal_write_waits_for_owned_write_and_blocks_late_admission() {
        let location = memfd_location("seal-write-reservation", 4096);
        let state = state_for_location(&location).unwrap();
        let write = begin_write(&location, 1).unwrap();
        write.admit_write(&location, 4096, 0, 1).unwrap();
        assert_eq!(state.state.lock().active_writes, 1);

        let (result_tx, result_rx) = mpsc::channel();
        let sealing = std::thread::spawn({
            let state = state.clone();
            move || result_tx.send(state.add_seals(F_SEAL_WRITE)).unwrap()
        });
        while !state.state.lock().seal_update_active {
            std::thread::yield_now();
        }
        assert!(result_rx.try_recv().is_err());
        let (late_tx, late_rx) = mpsc::channel();
        let late_write = std::thread::spawn({
            let location = location.clone();
            move || {
                let reservation = begin_write(&location, 1).unwrap();
                let admission = reservation.admit_write(&location, 4096, 0, 1);
                late_tx.send(admission).unwrap();
            }
        });
        assert!(late_rx.try_recv().is_err());

        drop(write);
        assert_eq!(
            result_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(F_SEAL_WRITE)
        );
        sealing.join().unwrap();
        assert_eq!(
            late_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(AxError::OperationNotPermitted)
        );
        late_write.join().unwrap();
        assert_eq!(state.state.lock().active_writes, 0);

        let rejected = begin_write(&location, 1).unwrap();
        assert_eq!(
            rejected.admit_write(&location, 4096, 0, 1),
            Err(AxError::OperationNotPermitted)
        );
    }

    #[test]
    fn failed_seal_wait_rolls_back_pending_publication() {
        let state = Arc::new(MemfdState::new(true));
        let write = state.reserve_write().unwrap();
        state.changed.fail_next_wait();

        assert_eq!(state.add_seals(F_SEAL_WRITE), Err(AxError::ResourceBusy));
        {
            let snapshot = state.state.lock();
            assert_eq!(snapshot.seals, 0);
            assert_eq!(snapshot.pending_seals, 0);
            assert!(!snapshot.seal_update_active);
            assert_eq!(snapshot.active_writes, 1);
        }

        drop(write);
        assert_eq!(state.add_seals(F_SEAL_WRITE).unwrap(), F_SEAL_WRITE);
    }

    #[test]
    fn resize_waits_for_active_write_without_holding_the_state_spinlock() {
        let state = Arc::new(MemfdState::new(true));
        let write = state.reserve_write().unwrap();
        let started = Arc::new(Barrier::new(2));
        let (guard_tx, guard_rx) = mpsc::channel();
        let resize = std::thread::spawn({
            let state = state.clone();
            let started = started.clone();
            move || {
                started.wait();
                assert!(guard_tx.send(state.reserve_resize()).is_ok());
            }
        });
        started.wait();
        assert!(guard_rx.try_recv().is_err());

        drop(write);
        let resize_guard = guard_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(state.state.lock().resize_active);
        drop(resize_guard);
        resize.join().unwrap();
        assert!(!state.state.lock().resize_active);
    }

    #[test]
    fn pending_grow_seal_does_not_make_an_in_place_write_fail_or_hold_append_lock() {
        let location = memfd_location("pending-grow-reservation", 4096);
        let state = state_for_location(&location).unwrap();
        let append_domain = Arc::new(RwLock::new(()));
        let first = begin_write(&location, 1).unwrap();
        first.admit_write(&location, 4096, 0, 1).unwrap();

        let (seal_tx, seal_rx) = mpsc::channel();
        let sealing = std::thread::spawn({
            let state = state.clone();
            move || seal_tx.send(state.add_seals(F_SEAL_GROW)).unwrap()
        });
        while !state.state.lock().seal_update_active {
            std::thread::yield_now();
        }

        let (started_tx, started_rx) = mpsc::channel();
        let (late_tx, late_rx) = mpsc::channel();
        let late = std::thread::spawn({
            let append_domain = append_domain.clone();
            let location = location.clone();
            move || {
                started_tx.send(()).unwrap();
                // Reservation precedes the append domain by API contract.
                let reservation = begin_write(&location, 1).unwrap();
                let _append = append_domain.write().unwrap();
                late_tx
                    .send(reservation.admit_write(&location, 4096, 0, 1))
                    .unwrap();
            }
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(late_rx.try_recv().is_err());

        // An already-admitted writer can still enter the append/backend domain;
        // the late writer is sleeping before that lock, so no A/S/B cycle forms.
        let append = append_domain.read().unwrap();
        drop(first);
        drop(append);

        assert_eq!(
            seal_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(F_SEAL_GROW)
        );
        assert_eq!(
            late_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(())
        );
        sealing.join().unwrap();
        late.join().unwrap();
    }

    #[test]
    fn pending_grow_seal_allows_a_late_shrink_resize_after_publication() {
        let location = memfd_location("pending-grow-shrink", 4096);
        let state = state_for_location(&location).unwrap();
        let first = begin_write(&location, 1).unwrap();
        first.admit_write(&location, 4096, 0, 1).unwrap();

        let (seal_tx, seal_rx) = mpsc::channel();
        let sealing = std::thread::spawn({
            let state = state.clone();
            move || seal_tx.send(state.add_seals(F_SEAL_GROW)).unwrap()
        });
        while !state.state.lock().seal_update_active {
            std::thread::yield_now();
        }

        let (resize_tx, resize_rx) = mpsc::channel();
        let resizing = std::thread::spawn({
            let location = location.clone();
            move || assert!(resize_tx.send(begin_resize(&location, 2048)).is_ok())
        });
        assert!(resize_rx.try_recv().is_err());
        drop(first);

        assert_eq!(
            seal_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Ok(F_SEAL_GROW)
        );
        let resize = resize_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        drop(resize);
        sealing.join().unwrap();
        resizing.join().unwrap();
    }

    #[test]
    fn resize_and_compound_write_resize_check_distinct_seal_classes() {
        let shrink = memfd_location("seal-shrink-reservation", 4096);
        add_seals(&shrink, true, F_SEAL_SHRINK).unwrap();
        assert!(matches!(
            begin_resize(&shrink, 2048),
            Err(AxError::OperationNotPermitted)
        ));
        assert!(begin_resize(&shrink, 8192).is_ok());

        let grow = memfd_location("seal-grow-reservation", 4096);
        add_seals(&grow, true, F_SEAL_GROW).unwrap();
        assert!(matches!(
            begin_resize(&grow, 8192),
            Err(AxError::OperationNotPermitted)
        ));
        assert!(begin_resize(&grow, 2048).is_ok());

        let write = memfd_location("seal-compound-reservation", 4096);
        add_seals(&write, true, F_SEAL_WRITE).unwrap();
        assert!(matches!(
            begin_write_resize(&write, 0, 1024, 2048),
            Err(AxError::OperationNotPermitted)
        ));
    }
}
