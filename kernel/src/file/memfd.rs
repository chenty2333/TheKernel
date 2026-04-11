use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::Location;
use linux_raw_sys::general::{
    F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, MFD_ALLOW_SEALING, MFD_CLOEXEC,
};

pub(crate) const MEMFD_SUPPORTED_CREATE_FLAGS: u32 = MFD_CLOEXEC | MFD_ALLOW_SEALING;
pub(crate) const MEMFD_SUPPORTED_SEALS: u32 =
    F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE;

#[derive(Debug)]
pub(crate) struct MemfdState {
    seals: AtomicU32,
    writable_shared_mappings: AtomicUsize,
}

impl MemfdState {
    fn new(allow_sealing: bool) -> Self {
        let initial = if allow_sealing { 0 } else { F_SEAL_SEAL };
        Self {
            seals: AtomicU32::new(initial),
            writable_shared_mappings: AtomicUsize::new(0),
        }
    }

    fn seals(&self) -> u32 {
        self.seals.load(Ordering::Acquire)
    }

    fn add_writable_mapping(&self) {
        self.writable_shared_mappings.fetch_add(1, Ordering::AcqRel);
    }

    fn remove_writable_mapping(&self) {
        self.writable_shared_mappings.fetch_sub(1, Ordering::AcqRel);
    }

    fn has_writable_mapping(&self) -> bool {
        self.writable_shared_mappings.load(Ordering::Acquire) != 0
    }
}

#[derive(Debug)]
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

    pub(crate) fn set_active(&self, active: bool) {
        let previous = self.active.swap(active, Ordering::AcqRel);
        match (previous, active) {
            (false, true) => self.state.add_writable_mapping(),
            (true, false) => self.state.remove_writable_mapping(),
            _ => {}
        }
    }
}

impl Drop for WritableMappingRegistration {
    fn drop(&mut self) {
        if self.active.load(Ordering::Acquire) {
            self.state.remove_writable_mapping();
        }
    }
}

pub(crate) fn install_memfd_state(loc: &Location, allow_sealing: bool) -> Arc<MemfdState> {
    let mut guard = loc.user_data();
    guard.get_or_insert_with(|| MemfdState::new(allow_sealing))
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

    if seals & F_SEAL_WRITE != 0 && state.has_writable_mapping() {
        return Err(AxError::ResourceBusy);
    }

    let mut current = state.seals();
    loop {
        if current & F_SEAL_SEAL != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        let updated = current | seals;
        match state.seals.compare_exchange(
            current,
            updated,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return Ok(updated),
            Err(observed) => current = observed,
        }
    }
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
