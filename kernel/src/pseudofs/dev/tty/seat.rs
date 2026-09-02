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

/// A single-seat coordinator.  The mutex protects only the phase and ticket;
/// no hook is ever invoked while it is held.
pub(crate) struct SeatLifecycle {
    /// Serializes slow hook execution.  `abort_current` may still invalidate
    /// its generation concurrently, but no two ordinary transitions can
    /// overlap their device/ACL effects.
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
    pub(crate) fn transition(&self, target: SeatTarget, hooks: &mut dyn SeatHooks) -> AxResult<()> {
        let _transaction = self.transaction.lock();
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

    /// Makes every in-flight ticket stale and restores fbcon through the
    /// supplied hook.  This is the owner-exit/compositor-crash/seatd-restart
    /// path and is safe to call repeatedly.
    pub(crate) fn abort_current(&self, hooks: &mut dyn SeatHooks) {
        // Abort is a slow transaction too.  Serializing it with transition
        // prevents a timeout/restart from interleaving fbcon restore with a
        // still-running release/acquire phase.
        let _transaction = self.transaction.lock();
        let ticket = {
            let mut state = self.state.lock();
            state.generation = state.generation.wrapping_add(1);
            state.phase = SeatPhase::PreparingAcquire;
            SeatTicket {
                generation: state.generation,
                target: state.active,
            }
        };
        hooks.abort(ticket.target);
        let mut state = self.state.lock();
        if state.generation == ticket.generation {
            state.active = SeatTarget::Fbcon;
            state.phase = SeatPhase::Fbcon;
        }
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
