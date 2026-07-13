use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::Location;
use axsync::spin::SpinNoIrq;
use linux_raw_sys::general::{
    F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, MFD_ALLOW_SEALING, MFD_CLOEXEC,
};

pub(crate) const MEMFD_SUPPORTED_CREATE_FLAGS: u32 = MFD_CLOEXEC | MFD_ALLOW_SEALING;
pub(crate) const MEMFD_SUPPORTED_SEALS: u32 =
    F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;

#[derive(Debug)]
struct MemfdSealState {
    seals: u32,
    writable_shared_mappings: usize,
}

pub(crate) struct MemfdState {
    state: SpinNoIrq<MemfdSealState>,
}

impl MemfdState {
    fn new(allow_sealing: bool) -> Self {
        let initial = if allow_sealing { 0 } else { F_SEAL_SEAL };
        Self {
            state: SpinNoIrq::new(MemfdSealState {
                seals: initial,
                writable_shared_mappings: 0,
            }),
        }
    }

    fn seals(&self) -> u32 {
        self.state.lock().seals
    }

    fn add_writable_mapping(&self) -> AxResult<()> {
        let mut state = self.state.lock();
        if state.seals & F_SEAL_WRITE != 0 {
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
        Ok(())
    }

    fn add_seals(&self, seals: u32) -> AxResult<u32> {
        let mut state = self.state.lock();
        if seals & F_SEAL_WRITE != 0 && state.writable_shared_mappings != 0 {
            return Err(AxError::ResourceBusy);
        }
        if state.seals & F_SEAL_SEAL != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        state.seals |= seals;
        Ok(state.seals)
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
    guard
        .try_get_or_insert_with(|| MemfdState::new(allow_sealing))
        .map_err(Into::into)
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

pub(crate) fn check_write(loc: &Location, offset: u64, len: usize) -> AxResult<()> {
    if len == 0 {
        return Ok(());
    }
    let Some(seals) = current_seals(loc) else {
        return Ok(());
    };
    if seals & F_SEAL_WRITE != 0 {
        return Err(AxError::OperationNotPermitted);
    }
    let end = offset
        .checked_add(len as u64)
        .ok_or(AxError::InvalidInput)?;
    if seals & F_SEAL_GROW != 0 && end > loc.len()? {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
}

pub(crate) fn check_resize(loc: &Location, new_len: u64) -> AxResult<()> {
    let Some(seals) = current_seals(loc) else {
        return Ok(());
    };
    let old_len = loc.len()?;
    if new_len < old_len && seals & F_SEAL_SHRINK != 0 {
        return Err(AxError::OperationNotPermitted);
    }
    if new_len > old_len && seals & F_SEAL_GROW != 0 {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(())
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
    extern crate std;

    use super::*;

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
}
