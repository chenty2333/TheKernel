//! Reverse mappings for shared-page backings.
//!
//! The registry intentionally holds only weak address-space references.  A
//! VMA backend owns its [`AliasLease`], so dropping the last VMA cannot leave
//! the registry keeping an address space (and, through it, the VMA) alive.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axsync::Mutex;
use thekernel_linux_mm::AddressSpaceId;

use super::AddrSpace;

/// Process-independent identity of one `SharedPages` allocation.
///
/// This is deliberately not an allocation address: a stale reverse-map entry
/// must never become an alias for an unrelated backing after allocator reuse.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SharedBackingKey(u64);

impl SharedBackingKey {
    pub(crate) fn allocate() -> AxResult<Self> {
        let key = NEXT_SHARED_BACKING_KEY
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| AxError::ResourceBusy)?;
        Ok(Self(key))
    }
}

static NEXT_SHARED_BACKING_KEY: AtomicU64 = AtomicU64::new(1);
static NEXT_ALIAS_LEASE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct AliasEntry {
    lease_id: u64,
    address_space_id: AddressSpaceId,
    address_space: Weak<Mutex<AddrSpace>>,
    state: AliasState,
}

/// A registration is not visible to cross-mm walkers until its VMA is
/// published.  Walkers retry instead of taking a snapshot across that gap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AliasState {
    Pending { generation: u64 },
    Committed { generation: u64 },
}

#[derive(Default)]
struct AliasRegistry {
    entries: BTreeMap<SharedBackingKey, Vec<AliasEntry>>,
    mutations: BTreeMap<SharedBackingKey, u64>,
}

#[cfg(not(test))]
type AliasRegistryMutex<T> = Mutex<T>;
#[cfg(test)]
type AliasRegistryMutex<T> = spin::Mutex<T>;

static ALIASES: AliasRegistryMutex<AliasRegistry> = AliasRegistryMutex::new(AliasRegistry {
    entries: BTreeMap::new(),
    mutations: BTreeMap::new(),
});

fn allocate_lease_id() -> AxResult<u64> {
    NEXT_ALIAS_LEASE_ID
        .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| AxError::ResourceBusy)
}

/// Backend-owned registration of one address space as an alias of a shared
/// backing.  The weak reference prevents a registry-to-mm strong cycle.
#[must_use = "dropping the lease unregisters the alias"]
pub(crate) struct AliasLease {
    address_space: Weak<Mutex<AddrSpace>>,
    address_space_id: AddressSpaceId,
    key: SharedBackingKey,
    lease_id: u64,
}

/// A pre-publication reverse-map registration.  Dropping it aborts the
/// registration, while [`Self::commit`] makes the exact same generation live.
#[must_use = "a pending alias registration must be committed or aborted"]
pub(crate) struct PendingAliasLease(Option<AliasLease>);

/// Excludes new alias publication for one backing while a cross-mm folio
/// mutation owns a stable participant set.
#[must_use = "dropping the reservation reopens alias publication"]
pub(crate) struct AliasMutationReservation {
    key: SharedBackingKey,
    generation: u64,
}

impl AliasLease {
    /// Registers an address-space alias before a backend exposes it.
    pub(crate) fn try_new(
        key: SharedBackingKey,
        address_space: &Arc<Mutex<AddrSpace>>,
        address_space_id: AddressSpaceId,
    ) -> AxResult<Self> {
        PendingAliasLease::try_prepare(key, address_space, address_space_id)?
            .commit()
            .ok_or(AxError::BadState)
    }

    pub(crate) const fn key(&self) -> SharedBackingKey {
        self.key
    }

    pub(crate) const fn address_space_id(&self) -> AddressSpaceId {
        self.address_space_id
    }
}

impl PendingAliasLease {
    /// Attempts one pending-generation admission without waiting.  Fork uses
    /// this form so it can drop the parent mm lock before retrying a backing
    /// currently frozen by a cross-mm folio transaction.
    pub(crate) fn try_prepare(
        key: SharedBackingKey,
        address_space: &Arc<Mutex<AddrSpace>>,
        address_space_id: AddressSpaceId,
    ) -> AxResult<Self> {
        let lease_id = allocate_lease_id()?;
        let weak = Arc::downgrade(address_space);
        let mut registry = ALIASES.lock();
        if registry.mutations.contains_key(&key) {
            return Err(AxError::WouldBlock);
        }
        let aliases = registry.entries.entry(key).or_default();
        if let Some(existing) = aliases.iter().find(|entry| {
            entry.address_space_id == address_space_id && entry.address_space.ptr_eq(&weak)
        }) {
            return match existing.state {
                AliasState::Pending { .. } => Err(AxError::WouldBlock),
                AliasState::Committed { .. } => Ok(Self(None)),
            };
        }
        aliases.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        aliases.push(AliasEntry {
            lease_id,
            address_space_id,
            address_space: weak.clone(),
            state: AliasState::Pending {
                generation: lease_id,
            },
        });
        Ok(Self(Some(AliasLease {
            address_space: weak,
            address_space_id,
            key,
            lease_id,
        })))
    }

    pub(crate) fn commit(self) -> Option<AliasLease> {
        let lease = self.0?;
        let mut registry = ALIASES.lock();
        let entry = registry
            .entries
            .get_mut(&lease.key)
            .and_then(|aliases| {
                aliases
                    .iter_mut()
                    .find(|entry| entry.lease_id == lease.lease_id)
            })
            .expect("pending alias disappeared before commit");
        entry.state = AliasState::Committed {
            generation: lease.lease_id,
        };
        Some(lease)
    }
}

/// Waits until a mapper may retry alias admission for `key`.
///
/// Call only after dropping the mm, IPC, and backing locks: a cross-mm folio
/// mutation can require any of them before it releases its reservation.
pub(crate) fn wait_for_alias_publication(key: SharedBackingKey) {
    loop {
        let registry = ALIASES.lock();
        let blocked = registry.mutations.contains_key(&key)
            || registry.entries.get(&key).is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|entry| matches!(entry.state, AliasState::Pending { .. }))
            });
        drop(registry);
        if !blocked {
            return;
        }
        core::hint::spin_loop();
    }
}

impl Drop for AliasMutationReservation {
    fn drop(&mut self) {
        let mut registry = ALIASES.lock();
        if registry.mutations.remove(&self.key) != Some(self.generation) {
            panic!("alias mutation reservation generation changed");
        }
    }
}

impl Drop for AliasLease {
    fn drop(&mut self) {
        let mut registry = ALIASES.lock();
        let Some(aliases) = registry.entries.get_mut(&self.key) else {
            return;
        };
        if let Some(index) = aliases
            .iter()
            .position(|entry| entry.lease_id == self.lease_id)
        {
            aliases.swap_remove(index);
        }
        if aliases.is_empty() {
            registry.entries.remove(&self.key);
        }
    }
}

/// A lock-free-to-consume alias snapshot.  Call [`Self::revalidate`] before
/// operating on its address space: its lease may have been dropped after the
/// snapshot was made.
#[derive(Clone)]
pub(crate) struct AliasSnapshot {
    key: SharedBackingKey,
    lease_id: u64,
    address_space_id: AddressSpaceId,
    address_space: Weak<Mutex<AddrSpace>>,
}

impl AliasSnapshot {
    pub(crate) const fn address_space_id(&self) -> AddressSpaceId {
        self.address_space_id
    }

    /// Returns a live address space only if this exact lease is still
    /// registered and the immutable mm identity still agrees.
    pub(crate) fn revalidate(&self) -> Option<Arc<Mutex<AddrSpace>>> {
        let registered = {
            let registry = ALIASES.lock();
            registry.entries.get(&self.key).is_some_and(|aliases| {
                aliases.iter().any(|entry| {
                    entry.lease_id == self.lease_id
                        && entry.address_space_id == self.address_space_id
                        && entry.address_space.ptr_eq(&self.address_space)
                })
            })
        };
        if !registered {
            return None;
        }
        let address_space = self.address_space.upgrade()?;
        let identity_matches = address_space.lock().address_space_id() == self.address_space_id;
        identity_matches.then_some(address_space)
    }
}

/// Takes a deterministic snapshot of all current aliases for `key`.
///
/// Results are sorted by `AddressSpaceId`, then registration order.  Expired
/// weak entries are omitted; their leases will remove them when their backend
/// is destroyed.
pub(crate) fn snapshot_aliases(key: SharedBackingKey) -> Vec<AliasSnapshot> {
    // A mapper installs the pending generation while it still owns its mm
    // lock.  Never try to acquire that mm here: release the registry and
    // retry, so publication/abort can make progress without lock inversion.
    let registry = loop {
        let registry = ALIASES.lock();
        if !registry.entries.get(&key).is_some_and(|aliases| {
            aliases
                .iter()
                .any(|entry| matches!(entry.state, AliasState::Pending { .. }))
        }) {
            break registry;
        }
        drop(registry);
        core::hint::spin_loop();
    };
    let mut snapshot = registry.entries.get(&key).map_or_else(Vec::new, |aliases| {
        aliases
            .iter()
            .filter(|entry| matches!(entry.state, AliasState::Committed { .. }))
            .filter(|entry| entry.address_space.strong_count() != 0)
            .map(|entry| AliasSnapshot {
                key,
                lease_id: entry.lease_id,
                address_space_id: entry.address_space_id,
                address_space: entry.address_space.clone(),
            })
            .collect()
    });
    snapshot.sort_unstable_by(|lhs, rhs| {
        lhs.address_space_id
            .cmp(&rhs.address_space_id)
            .then_with(|| lhs.lease_id.cmp(&rhs.lease_id))
    });
    snapshot
}

/// Freezes one backing's alias topology and returns its committed aliases.
/// The caller holds the reservation through every participant lock, PTE
/// publication, and backing ownership commit.  A mapper that arrives after
/// the snapshot waits before it can create a pending generation, closing the
/// otherwise unavoidable snapshot-to-commit re-promotion window.
pub(crate) fn reserve_alias_mutation(
    key: SharedBackingKey,
) -> (AliasMutationReservation, Vec<AliasSnapshot>) {
    loop {
        let mut registry = ALIASES.lock();
        if registry.mutations.contains_key(&key)
            || registry.entries.get(&key).is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|entry| matches!(entry.state, AliasState::Pending { .. }))
            })
        {
            drop(registry);
            core::hint::spin_loop();
            continue;
        }
        let generation = NEXT_ALIAS_LEASE_ID.load(Ordering::Acquire);
        registry.mutations.insert(key, generation);
        let mut snapshot = registry.entries.get(&key).map_or_else(Vec::new, |aliases| {
            aliases
                .iter()
                .filter(|entry| matches!(entry.state, AliasState::Committed { .. }))
                .filter(|entry| entry.address_space.strong_count() != 0)
                .map(|entry| AliasSnapshot {
                    key,
                    lease_id: entry.lease_id,
                    address_space_id: entry.address_space_id,
                    address_space: entry.address_space.clone(),
                })
                .collect()
        });
        snapshot.sort_unstable_by(|lhs, rhs| {
            lhs.address_space_id
                .cmp(&rhs.address_space_id)
                .then_with(|| lhs.lease_id.cmp(&rhs.lease_id))
        });
        return (AliasMutationReservation { key, generation }, snapshot);
    }
}

#[cfg(test)]
mod tests {
    use memory_addr::VirtAddr;

    use super::*;

    #[test]
    fn registry_keeps_one_lease_per_mm_and_backing() {
        let address_space = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), 0x20_000).unwrap(),
        ));
        let address_space_id = address_space.lock().address_space_id();
        let key = SharedBackingKey::allocate().unwrap();
        let first = AliasLease::try_new(key, &address_space, address_space_id).unwrap();
        assert!(matches!(
            AliasLease::try_new(key, &address_space, address_space_id),
            Err(AxError::BadState)
        ));

        let snapshot = snapshot_aliases(key);
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot.iter().all(|alias| alias.revalidate().is_some()));

        drop(first);
        assert!(snapshot_aliases(key).is_empty());
    }
}
