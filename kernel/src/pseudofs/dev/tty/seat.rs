//! Generation-checked seat ownership transactions.
//!
//! VT state is deliberately not held while these hooks run: device teardown
//! can wake waiters, revoke file descriptions, and touch VFS ACL state.  The
//! generation ticket makes a late callback from a refused VT_PROCESS release,
//! a dead compositor, or a restarted seat manager harmless.

use alloc::vec::Vec;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::Location;
use axsync::Mutex;
use lazy_static::lazy_static;
use thekernel_linux_process_adapter::Pid;

use crate::file::posix_acl;

struct PublishedSeatNodes {
    primary: Vec<Location>,
    input: Vec<Location>,
    active_uid: Option<u32>,
}

lazy_static! {
    /// Locations are captured from the actual devfs open path.  They are VFS
    /// objects, not reconstructed pathname strings, so ACL mutations affect
    /// the inode clients will subsequently open.
    static ref PUBLISHED_NODES: Mutex<PublishedSeatNodes> = Mutex::new(PublishedSeatNodes {
        primary: Vec::new(), input: Vec::new(), active_uid: None,
    });
}

fn remember(list: &mut Vec<Location>, location: &Location) -> AxResult<bool> {
    // A device node may be opened repeatedly.  Keeping duplicate locations
    // would be harmless but makes rollback needlessly long; pointer/path
    // identity is VFS-private, so preserve the first canonical handle.
    if list.iter().any(|known| known.ptr_eq(location)) {
        return Ok(false);
    }
    if list.len() == 128 {
        return Err(AxError::NoMemory);
    }
    list.try_reserve(1).map_err(|_| AxError::NoMemory)?;
    list.push(location.clone());
    Ok(true)
}

pub(crate) fn remember_primary_node(location: &Location) -> AxResult<()> {
    let mut nodes = PUBLISHED_NODES.lock();
    let inserted = remember(&mut nodes.primary, location)?;
    if inserted
        && let Some(uid) = nodes.active_uid
        && let Err(error) = posix_acl::grant_user(location, uid, 0o6)
    {
        nodes.primary.retain(|known| !known.ptr_eq(location));
        return Err(error);
    }
    Ok(())
}

pub(crate) fn remember_input_node(location: &Location) -> AxResult<()> {
    let mut nodes = PUBLISHED_NODES.lock();
    let inserted = remember(&mut nodes.input, location)?;
    if inserted
        && let Some(uid) = nodes.active_uid
        && let Err(error) = posix_acl::grant_user(location, uid, 0o6)
    {
        // Do not make a node discoverable to the active compositor without
        // the matching ACL. Removing it from the publication set lets a
        // later open retry the complete remember+grant transaction.
        nodes.input.retain(|known| !known.ptr_eq(location));
        return Err(error);
    }
    Ok(())
}

pub(crate) fn grant_published_nodes(uid: u32) -> AxResult<()> {
    let mut nodes = PUBLISHED_NODES.lock();
    nodes.active_uid = Some(uid);
    let locations = nodes
        .primary
        .iter()
        .chain(nodes.input.iter())
        .cloned()
        .collect::<Vec<_>>();
    let mut granted = 0;
    for location in &locations {
        if let Err(error) = posix_acl::grant_user(location, uid, 0o6) {
            // Do not advertise a half-granted graphics seat.  Undo grants
            // made in this attempt before returning to the fbcon rollback.
            for granted_location in locations.iter().take(granted) {
                let _ = posix_acl::revoke_user(granted_location, uid);
            }
            nodes.active_uid = None;
            return Err(error);
        }
        granted += 1;
    }
    Ok(())
}

pub(crate) fn revoke_published_nodes(uid: u32) {
    let mut nodes = PUBLISHED_NODES.lock();
    if nodes.active_uid == Some(uid) {
        nodes.active_uid = None;
    }
    let locations = nodes
        .primary
        .iter()
        .chain(nodes.input.iter())
        .cloned()
        .collect::<Vec<_>>();
    for location in locations {
        if let Err(error) = posix_acl::revoke_user(&location, uid) {
            // Revocation failure never reopens the KMS/input gates.
            warn!("failed to revoke inactive-session device ACL: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SeatOwner {
    pub(crate) pid: Option<Pid>,
    /// The caller which enters KD_GRAPHICS supplies the UID used for the
    /// active VT's device ACL transaction.
    pub(crate) uid: Option<u32>,
    pub(crate) vt: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SeatTarget {
    Fbcon,
    Graphics(SeatOwner),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SeatPhase {
    Fbcon,
    PreparingRelease,
    Released,
    PreparingAcquire,
    Graphics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SeatTicket {
    generation: u64,
    target: SeatTarget,
}

struct SeatState {
    generation: u64,
    phase: SeatPhase,
    active: SeatTarget,
}

/// Hooks are intentionally small and transactional.  Implementations must
/// make release terminal for old device FDs before returning; acquire must
/// leave fbcon usable if it fails.
pub(crate) trait SeatHooks {
    fn prepare_release(&mut self, from: SeatTarget) -> AxResult<()>;
    fn release(&mut self, from: SeatTarget) -> AxResult<()>;
    fn prepare_acquire(&mut self, target: SeatTarget) -> AxResult<()>;
    fn acquire(&mut self, target: SeatTarget) -> AxResult<()>;
    fn abort(&mut self, target: SeatTarget);
}

/// A single-seat coordinator. The state mutex protects the phase and ticket;
/// hooks run under the transaction mutex with the state mutex released.
pub(crate) struct SeatLifecycle {
    /// Serializes target snapshots with slow hook execution so a delayed
    /// reconciliation cannot apply an obsolete VT owner after a newer one.
    transaction: Mutex<()>,
    state: Mutex<SeatState>,
}

impl SeatLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            transaction: Mutex::new(()),
            state: Mutex::new(SeatState {
                generation: 0,
                phase: SeatPhase::Fbcon,
                active: SeatTarget::Fbcon,
            }),
        }
    }

    pub(crate) fn active(&self) -> SeatTarget {
        self.state.lock().active
    }

    fn next_ticket(&self, target: SeatTarget) -> AxResult<SeatTicket> {
        let mut state = self.state.lock();
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or(AxError::InvalidInput)?;
        state.phase = SeatPhase::PreparingRelease;
        Ok(SeatTicket {
            generation: state.generation,
            target,
        })
    }

    fn current(&self, ticket: SeatTicket) -> bool {
        self.state.lock().generation == ticket.generation
    }

    /// Runs release followed by acquire.  Each phase is published before its
    /// callback, allowing owner exit, hot-unplug, and seatd restart to abort
    /// a stale generation without waiting on VT locks.
    pub(crate) fn transition(
        &self,
        snapshot_target: impl FnOnce() -> SeatTarget,
        hooks: &mut dyn SeatHooks,
    ) -> AxResult<()> {
        let _transaction = self.transaction.lock();
        // Sample VT ownership only after joining the serialized transaction.
        // The callback releases the VT spin lock before any device hook runs.
        let target = snapshot_target();
        if self.active() == target {
            return Ok(());
        }
        let ticket = self.next_ticket(target)?;
        let from = self.active();
        if let Err(error) = hooks.prepare_release(from) {
            self.abort(ticket, hooks);
            return Err(error);
        }
        if !self.current(ticket) {
            self.abort(ticket, hooks);
            return Err(AxError::Interrupted);
        }
        if let Err(error) = hooks.release(from) {
            self.abort(ticket, hooks);
            return Err(error);
        }
        {
            let mut state = self.state.lock();
            if state.generation != ticket.generation {
                drop(state);
                self.abort(ticket, hooks);
                return Err(AxError::Interrupted);
            }
            state.phase = SeatPhase::Released;
            state.phase = SeatPhase::PreparingAcquire;
        }
        if let Err(error) = hooks.prepare_acquire(target) {
            self.abort(ticket, hooks);
            return Err(error);
        }
        if !self.current(ticket) {
            self.abort(ticket, hooks);
            return Err(AxError::Interrupted);
        }
        if let Err(error) = hooks.acquire(target) {
            self.abort(ticket, hooks);
            return Err(error);
        }
        let mut state = self.state.lock();
        if state.generation != ticket.generation {
            drop(state);
            self.abort(ticket, hooks);
            return Err(AxError::Interrupted);
        }
        state.active = target;
        state.phase = match target {
            SeatTarget::Fbcon => SeatPhase::Fbcon,
            SeatTarget::Graphics(_) => SeatPhase::Graphics,
        };
        Ok(())
    }

    fn abort(&self, ticket: SeatTicket, hooks: &mut dyn SeatHooks) {
        if !self.current(ticket) {
            return;
        }
        hooks.abort(ticket.target);
        let mut state = self.state.lock();
        if state.generation == ticket.generation {
            state.active = SeatTarget::Fbcon;
            state.phase = SeatPhase::Fbcon;
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::sync::Arc;
    use std::sync::mpsc;

    use super::*;
    use crate::drm::{
        DisplayAdapter, DrmDevice, DrmError, DrmResult, DumbRequest, GemBacking, Scanout,
    };

    struct Adapter;
    impl DisplayAdapter for Adapter {
        fn create_dumb(&self, _: DumbRequest, _: u32, _: u64) -> DrmResult<Arc<dyn GemBacking>> {
            Err(DrmError::Unsupported)
        }
        fn present(&self, _: Scanout) -> DrmResult<Arc<crate::drm::fence::Fence>> {
            Err(DrmError::Unsupported)
        }
    }

    struct Hooks(Arc<DrmDevice>);
    impl SeatHooks for Hooks {
        fn prepare_release(&mut self, _: SeatTarget) -> AxResult<()> {
            self.0.suspend_kms_for_seat();
            Ok(())
        }
        fn release(&mut self, _: SeatTarget) -> AxResult<()> {
            Ok(())
        }
        fn prepare_acquire(&mut self, _: SeatTarget) -> AxResult<()> {
            Ok(())
        }
        fn acquire(&mut self, _: SeatTarget) -> AxResult<()> {
            self.0.resume_kms_for_seat();
            Ok(())
        }
        fn abort(&mut self, _: SeatTarget) {
            self.0.resume_kms_for_seat();
        }
    }

    #[test]
    fn delayed_reconciliation_preserves_new_graphics_device_lease() {
        let _scheduler = crate::test_support::scheduler_test_context();
        let seat = Arc::new(SeatLifecycle::new());
        let desired = Arc::new(spin::Mutex::new(SeatTarget::Fbcon));
        let device = DrmDevice::new(Arc::new(Adapter), 1, 2, 3, 4);
        let (arrived_tx, arrived_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let delayed = {
            let seat = seat.clone();
            let desired = desired.clone();
            let device = device.clone();
            std::thread::spawn(move || {
                // An unrelated exiting helper reaches reconciliation while
                // the selected VT is still text, then loses its CPU turn.
                assert_eq!(*desired.lock(), SeatTarget::Fbcon);
                arrived_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                seat.transition(
                    || {
                        // This is the crucial ordering property: sampling
                        // before transaction admission recreates the bug.
                        assert!(seat.transaction.try_lock().is_none());
                        *desired.lock()
                    },
                    &mut Hooks(device),
                )
                .unwrap();
            })
        };
        arrived_rx.recv().unwrap();
        let graphics = SeatTarget::Graphics(SeatOwner {
            pid: Some(75),
            uid: Some(100),
            vt: 1,
        });
        *desired.lock() = graphics;
        seat.transition(|| *desired.lock(), &mut Hooks(device.clone()))
            .unwrap();
        let compositor = device.open_primary();
        compositor.become_master().unwrap();
        // Resume only after the new session has acquired its actual DRM OFD.
        // Host tests share one emulated task, so scheduler-facing operations
        // stay ordered even though the delayed helper is a separate thread.
        resume_tx.send(()).unwrap();
        delayed.join().unwrap();
        assert_eq!(seat.active(), graphics);
        assert_eq!(compositor.become_master(), Ok(()));
    }
}
