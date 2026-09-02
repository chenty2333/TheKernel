//! Swap-device registry and slot allocator.
//!
//! Slots are globally unique and remain reserved until page-in or swapoff.
//! The MM can use this small interface without exposing VFS objects in PTEs.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axerrno::{AxError, AxResult, LinuxError};
use axfs::OpenOptions;
use axfs_ng_vfs::{Location, NodeType};
use axsync::Mutex;
use spin::Lazy;

use super::{AddrSpace, SharedPages};

const PAGE: usize = 4096;
const SWAP_SIGNATURE: &[u8] = b"SWAPSPACE2";
const SWAP_FLAG_PREFER: u32 = 0x8000;
const SWAP_FLAGS_VALID: u32 = 0x7ffff;

struct SwapArea {
    id: u16,
    location: Location,
    priority: i16,
    // One reference per software PTE naming the slot.  This is MM ownership,
    // not a transient I/O pin.
    refs: Vec<u32>,
    draining: bool,
    _activation: SwapActivation,
}
struct SwapRegistry {
    areas: BTreeMap<Vec<u8>, SwapArea>,
    next_id: u16,
}
impl Default for SwapRegistry {
    fn default() -> Self {
        Self {
            areas: BTreeMap::new(),
            // Zero is deliberately excluded from the software-PTE format.
            next_id: 1,
        }
    }
}
static SWAPS: Lazy<Mutex<SwapRegistry>> = Lazy::new(|| Mutex::new(SwapRegistry::default()));
// Bumped before a fork child can copy software swap PTEs. Swapoff samples it
// before snapshotting and aborts/rolls back if the live-MM population changes.
static LIVE_ADDRESS_SPACE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Per-inode mutation gate shared by VFS writers and shared writable VMAs.
/// The lock is the single linearization point: activation can only publish
/// after every pre-existing writer has gone away, and publication prevents a
/// later writer from entering.
pub(crate) struct MutationState {
    active: bool,
    writers: usize,
    writable_mappings: usize,
}

type MutationKey = (u64, u64, u64);
static MUTATION_STATES: Lazy<Mutex<BTreeMap<MutationKey, Weak<Mutex<MutationState>>>>> =
    Lazy::new(|| Mutex::new(BTreeMap::new()));

fn mutation_key(location: &Location) -> MutationKey {
    (
        location.mountpoint().mount_id(),
        location.mountpoint().device(),
        location.inode(),
    )
}

fn mutation_state(location: &Location) -> Arc<Mutex<MutationState>> {
    let key = mutation_key(location);
    let mut states = MUTATION_STATES.lock();
    if let Some(state) = states.get(&key).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(Mutex::new(MutationState {
        active: false,
        writers: 0,
        writable_mappings: 0,
    }));
    states.insert(key, Arc::downgrade(&state));
    state
}

pub(crate) fn mutation_state_for_mapping(location: &Location) -> Option<Arc<Mutex<MutationState>>> {
    (location.node_type() == NodeType::RegularFile).then(|| mutation_state(location))
}

/// Held across one actual inode content or size mutation.
pub(crate) struct MutationAdmission(Arc<Mutex<MutationState>>);
impl Drop for MutationAdmission {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.writers = state.writers.checked_sub(1).expect("swap writer underflow");
    }
}

/// Cloneable VMA-owned registration.  Its active bit is set before writable
/// PTE publication and cleared only after revocation, matching the backend's
/// existing writable-segment lifecycle.
pub(crate) struct WritableMappingRegistration {
    state: Arc<Mutex<MutationState>>,
    active: AtomicBool,
}
impl WritableMappingRegistration {
    pub(crate) fn for_location(location: &Location) -> Option<Self> {
        (location.node_type() == NodeType::RegularFile).then(|| Self {
            state: mutation_state(location),
            active: AtomicBool::new(false),
        })
    }
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub(crate) fn set_active(&self, active: bool) -> AxResult<()> {
        let mut state = self.state.lock();
        let previous = self.active.load(Ordering::Acquire);
        if previous == active {
            return Ok(());
        }
        if active {
            if state.active {
                return Err(LinuxError::EBUSY.into());
            }
            state.writable_mappings = state
                .writable_mappings
                .checked_add(1)
                .ok_or(AxError::NoMemory)?;
        } else {
            state.writable_mappings = state
                .writable_mappings
                .checked_sub(1)
                .ok_or(AxError::BadState)?;
        }
        self.active.store(active, Ordering::Release);
        Ok(())
    }
}
impl Drop for WritableMappingRegistration {
    fn drop(&mut self) {
        let _ = self.set_active(false);
    }
}

pub(crate) fn admit_mutation(location: &Location) -> AxResult<MutationAdmission> {
    let state = mutation_state(location);
    admit_mutation_state(state)
}

fn admit_mutation_state(state: Arc<Mutex<MutationState>>) -> AxResult<MutationAdmission> {
    let mut guard = state.lock();
    if guard.active {
        return Err(LinuxError::EBUSY.into());
    }
    guard.writers = guard.writers.checked_add(1).ok_or(AxError::NoMemory)?;
    drop(guard);
    Ok(MutationAdmission(state))
}

struct SwapActivation(Arc<Mutex<MutationState>>);
impl Drop for SwapActivation {
    fn drop(&mut self) {
        self.0.lock().active = false;
    }
}

fn admit_activation(location: &Location) -> AxResult<SwapActivation> {
    let state = mutation_state(location);
    admit_activation_state(state)
}

fn admit_activation_state(state: Arc<Mutex<MutationState>>) -> AxResult<SwapActivation> {
    let mut guard = state.lock();
    if guard.active || guard.writers != 0 || guard.writable_mappings != 0 {
        return Err(LinuxError::EBUSY.into());
    }
    guard.active = true;
    drop(guard);
    Ok(SwapActivation(state))
}

/// One published process image holds a registration. Weak entries let final
/// process teardown race safely with swapoff's snapshot without retaining an
/// otherwise-dead mm.
struct LiveAddressSpaces {
    entries: BTreeMap<u64, (Weak<Mutex<AddrSpace>>, usize, bool)>,
}

static LIVE_ADDRESS_SPACES: Lazy<Mutex<LiveAddressSpaces>> = Lazy::new(|| {
    Mutex::new(LiveAddressSpaces {
        entries: BTreeMap::new(),
    })
});

pub(crate) fn register_address_space(aspace: &Arc<Mutex<AddrSpace>>) {
    let id = aspace.lock().address_space_id().get();
    let mut live = LIVE_ADDRESS_SPACES.lock();
    match live.entries.get_mut(&id) {
        Some((weak, refs, pending)) => {
            *weak = Arc::downgrade(aspace);
            if *pending {
                *pending = false;
            } else {
                *refs = refs.saturating_add(1);
            }
        }
        None => {
            live.entries.insert(id, (Arc::downgrade(aspace), 1, false));
        }
    }
}

/// Makes a fork child visible to swapoff before copied software PTEs exist.
/// The subsequent `ProcessData` publication claims this one reservation
/// instead of creating a second reference.
pub(crate) fn register_pending_address_space(aspace: &Arc<Mutex<AddrSpace>>) {
    let id = aspace.lock().address_space_id().get();
    LIVE_ADDRESS_SPACES
        .lock()
        .entries
        .insert(id, (Arc::downgrade(aspace), 1, true));
    LIVE_ADDRESS_SPACE_EPOCH.fetch_add(1, Ordering::AcqRel);
}

pub(crate) fn unregister_address_space(aspace: &Arc<Mutex<AddrSpace>>) {
    let id = aspace.lock().address_space_id().get();
    let mut live = LIVE_ADDRESS_SPACES.lock();
    let remove = match live.entries.get_mut(&id) {
        Some((_, refs, _)) if *refs > 1 => {
            *refs -= 1;
            false
        }
        Some(_) => true,
        None => false,
    };
    if remove {
        live.entries.remove(&id);
    }
}

/// Snapshots every process image that currently owns an address space.
///
/// This is also the authoritative live-mm reverse map for facilities such as
/// system-wide uprobes.  Callers only retain strong references returned by the
/// snapshot; the registry itself remains weak and therefore cannot extend an
/// mm's lifetime.
pub(crate) fn live_address_spaces() -> Vec<Arc<Mutex<AddrSpace>>> {
    let mut live = LIVE_ADDRESS_SPACES.lock();
    let mut spaces = Vec::new();
    let stale: Vec<_> = live
        .entries
        .iter()
        .filter_map(|(id, (weak, ..))| {
            weak.upgrade().map_or(Some(*id), |aspace| {
                spaces.push(aspace);
                None
            })
        })
        .collect();
    for id in stale {
        live.entries.remove(&id);
    }
    spaces
}

/// Revokes resident PTEs for exactly one externally owned backing.  The live
/// process-image registry is a weak reverse map: fork, VMA split, remap, and
/// teardown need no duplicated lifetime bookkeeping because each operation
/// changes the authoritative `AddrSpace` area set before this scan observes
/// it.  Locks are acquired one address space at a time, never while holding a
/// device/lease registry lock.
pub(crate) fn revoke_shared_pages(pages: &Arc<SharedPages>) {
    for aspace in live_address_spaces() {
        aspace.lock().revoke_external_shared_pages(pages);
    }
}

pub(crate) fn revoke_external_shared_pages(pages: &Arc<SharedPages>) {
    revoke_shared_pages(pages)
}

/// A software PTE payload.  Bit 63 distinguishes it from a present PFN;
/// bits 48..62 name a swap area and bits 0..47 select its 4 KiB slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SwapPte(u64);
impl SwapPte {
    const TAG: u64 = 1 << 63;
    fn new(area: u16, slot: usize) -> AxResult<Self> {
        (slot < (1usize << 48))
            .then_some(Self(Self::TAG | ((area as u64) << 48) | slot as u64))
            .ok_or(AxError::InvalidInput)
    }
    pub(crate) fn area(self) -> u16 {
        ((self.0 >> 48) & 0x7fff) as u16
    }
    pub(crate) fn slot(self) -> usize {
        (self.0 & ((1 << 48) - 1)) as usize
    }
    pub fn raw(self) -> u64 {
        self.0
    }
}

fn path_key(location: &Location) -> AxResult<Vec<u8>> {
    let path = location.absolute_path()?;
    let mut key = Vec::new();
    key.try_reserve(path.as_bytes().len())
        .map_err(|_| AxError::NoMemory)?;
    key.extend_from_slice(path.as_bytes());
    Ok(key)
}

/// Validates a Linux v1 swap header and publishes an empty slot map atomically.
pub fn activate(location: Location, flags: i32) -> AxResult<()> {
    if (flags as u32) & !SWAP_FLAGS_VALID != 0 || location.node_type() != NodeType::RegularFile {
        return Err(AxError::InvalidInput);
    }
    let activation = admit_activation(&location)?;
    let length = location.len()? as usize;
    if length < PAGE * 2 || !length.is_multiple_of(PAGE) {
        return Err(AxError::InvalidInput);
    }
    let mut header = [0u8; PAGE];
    let file = OpenOptions::new()
        .read(true)
        .open_loc(location.clone())?
        .into_file()?;
    if file.read_at(&mut header[..], 0)? != PAGE
        || &header[PAGE - SWAP_SIGNATURE.len()..] != SWAP_SIGNATURE
    {
        return Err(AxError::InvalidInput);
    }
    let key = path_key(&location)?;
    let slots = length / PAGE - 1;
    let mut swaps = SWAPS.lock();
    if swaps
        .areas
        .values()
        .any(|area| area.location.same_node(&location))
    {
        return Err(LinuxError::EBUSY.into());
    }
    let mut refs = Vec::new();
    refs.try_reserve_exact(slots)
        .map_err(|_| AxError::NoMemory)?;
    refs.resize(slots, 0);
    let id = swaps.next_id;
    swaps.next_id = swaps.next_id.wrapping_add(1);
    if id == 0 || swaps.areas.values().any(|area| area.id == id) {
        return Err(LinuxError::ENOSPC.into());
    }
    let flags = flags as u32;
    let priority = if flags & SWAP_FLAG_PREFER != 0 {
        (flags & 0x7fff) as i16
    } else {
        swaps
            .areas
            .values()
            .filter(|area| area.priority < 0)
            .map(|area| area.priority)
            .min()
            .unwrap_or(0)
            .saturating_sub(1)
    };
    swaps.areas.insert(
        key,
        SwapArea {
            id,
            location,
            priority,
            refs,
            draining: false,
            _activation: activation,
        },
    );
    Ok(())
}

/// Removes an inactive-only area.  A non-empty area is never torn down: that
/// is the rollback boundary until anonymous-page migration is complete.
pub fn deactivate(location: &Location) -> AxResult<()> {
    let (key, id) = {
        let mut swaps = SWAPS.lock();
        let key = swaps
            .areas
            .iter()
            .find_map(|(key, area)| area.location.same_node(location).then_some(key.clone()))
            .ok_or(AxError::InvalidInput)?;
        let area = swaps.areas.get_mut(&key).ok_or(AxError::InvalidInput)?;
        if area.draining {
            return Err(LinuxError::EBUSY.into());
        }
        area.draining = true;
        (key, area.id)
    };
    let epoch = LIVE_ADDRESS_SPACE_EPOCH.load(Ordering::Acquire);
    let mut spaces = live_address_spaces();
    spaces.sort_by_key(|aspace| aspace.lock().address_space_id().get());
    // Snapshot under each mm lock, then release every lock before the
    // allocation and I/O preflight. Each snapshot owns an additional slot
    // reference, preventing an intervening fault from invalidating its read.
    let prepared: AxResult<Vec<Vec<crate::mm::PreparedSwapoffPage>>> = spaces
        .iter()
        .map(|aspace| {
            let pages = aspace.lock().snapshot_swapoff_area(id)?;
            pages.into_iter().map(|page| page.prepare()).collect()
        })
        .collect();
    let mut prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(area) = SWAPS.lock().areas.get_mut(&key) {
                area.draining = false;
            }
            return Err(error);
        }
    };
    // Acquire every live mm in stable address-space-ID order. Validation is
    // global and side-effect-free; once it passes this commit phase contains
    // only preallocated page-table publication and reference transfers.
    let mut guards: Vec<_> = spaces.iter().map(|aspace| aspace.lock()).collect();
    if LIVE_ADDRESS_SPACE_EPOCH.load(Ordering::Acquire) != epoch {
        drop(guards);
        drop(prepared);
        if let Some(area) = SWAPS.lock().areas.get_mut(&key) {
            area.draining = false;
        }
        return Err(LinuxError::EBUSY.into());
    }
    if let Some(error) = guards
        .iter()
        .zip(prepared.iter())
        .find_map(|(aspace, pages)| aspace.validate_swapoff_pages(pages).err())
    {
        drop(guards);
        drop(prepared);
        if let Some(area) = SWAPS.lock().areas.get_mut(&key) {
            area.draining = false;
        }
        return Err(error);
    }
    for (aspace, pages) in guards.iter_mut().zip(prepared.iter_mut()) {
        aspace.commit_swapoff_pages(pages);
    }
    drop(guards);
    drop(prepared);
    let mut swaps = SWAPS.lock();
    let area = swaps.areas.get(&key).ok_or(AxError::InvalidInput)?;
    if area.refs.iter().any(|refs| *refs != 0) {
        drop(swaps);
        if let Some(area) = SWAPS.lock().areas.get_mut(&key) {
            area.draining = false;
        }
        return Err(LinuxError::EBUSY.into());
    }
    swaps.areas.remove(&key);
    Ok(())
}

/// Allocates a slot from the highest-priority active area.  The returned pair
/// is stable until `free_slot`; callers use it as the swap-PTE payload.
pub fn allocate_slot() -> AxResult<(Vec<u8>, usize)> {
    let mut swaps = SWAPS.lock();
    let key = swaps
        .areas
        .iter()
        .filter(|(_, area)| !area.draining && area.refs.contains(&0))
        .max_by_key(|(_, area)| area.priority)
        .map(|(key, _)| key.clone())
        .ok_or(LinuxError::ENOSPC)?;
    let area = swaps.areas.get_mut(&key).ok_or(AxError::Io)?;
    let slot = area
        .refs
        .iter()
        .position(|refs| *refs == 0)
        .ok_or(LinuxError::ENOSPC)?;
    area.refs[slot] = 1;
    Ok((key, slot))
}

fn release_slot(area: &[u8], slot: usize) -> AxResult<()> {
    let mut swaps = SWAPS.lock();
    let area = swaps.areas.get_mut(area).ok_or(AxError::InvalidInput)?;
    let refs = area.refs.get_mut(slot).ok_or(AxError::InvalidInput)?;
    if *refs == 0 {
        return Err(AxError::InvalidInput);
    }
    *refs -= 1;
    Ok(())
}

/// Takes another MM ownership reference after fork has copied a software PTE.
pub(crate) fn retain(entry: SwapPte) -> AxResult<()> {
    let mut swaps = SWAPS.lock();
    let area = swaps
        .areas
        .values_mut()
        .find(|area| area.id == entry.area())
        .ok_or(AxError::InvalidInput)?;
    let refs = area
        .refs
        .get_mut(entry.slot())
        .ok_or(AxError::InvalidInput)?;
    *refs = refs.checked_add(1).ok_or(AxError::NoMemory)?;
    Ok(())
}

/// Drops one software-PTE ownership reference, returning the slot to the free
/// map only after its last mapping has disappeared.
pub(crate) fn release(entry: SwapPte) -> AxResult<()> {
    let key = {
        let swaps = SWAPS.lock();
        swaps
            .areas
            .iter()
            .find(|(_, area)| area.id == entry.area())
            .map(|(key, _)| key.clone())
            .ok_or(AxError::InvalidInput)?
    };
    release_slot(&key, entry.slot())
}

pub fn active_area(location: &Location) -> AxResult<bool> {
    Ok(SWAPS
        .lock()
        .areas
        .values()
        .any(|area| area.location.same_node(location)))
}

/// Active swap backing is immutable. This compares the VFS node rather than
/// its spelling, so hard links and renames cannot bypass the exclusion.
pub(crate) fn check_not_active(location: &Location) -> AxResult<()> {
    (!active_area(location)?)
        .then_some(())
        .ok_or_else(|| LinuxError::EBUSY.into())
}

/// Writes one anonymous page to a reserved slot.  Allocation is rolled back
/// if VFS I/O fails, so a failed reclaim never leaks capacity or a PTE value.
pub fn pageout(page: &[u8]) -> AxResult<SwapPte> {
    if page.len() != PAGE {
        return Err(AxError::InvalidInput);
    }
    let (key, slot) = allocate_slot()?;
    let (location, id) = {
        let swaps = SWAPS.lock();
        let area = swaps.areas.get(&key).ok_or(AxError::Io)?;
        (area.location.clone(), area.id)
    };
    let result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .open_loc(location)?
            .into_file()?;
        (file.write_at(page, ((slot + 1) * PAGE) as u64)? == PAGE)
            .then_some(())
            .ok_or(AxError::Io)
    })();
    if let Err(error) = result {
        let _ = release_slot(&key, slot);
        return Err(error);
    }
    SwapPte::new(id, slot)
}

/// Reads a page without changing its MM ownership reference.
pub(crate) fn read(entry: SwapPte, page: &mut [u8]) -> AxResult<()> {
    if page.len() != PAGE {
        return Err(AxError::InvalidInput);
    }
    let (_key, location) = {
        let swaps = SWAPS.lock();
        let (key, area) = swaps
            .areas
            .iter()
            .find(|(_, area)| area.id == entry.area())
            .ok_or(AxError::InvalidInput)?;
        if area.refs.get(entry.slot()).copied().unwrap_or(0) == 0 {
            return Err(AxError::InvalidInput);
        }
        (key.clone(), area.location.clone())
    };
    let file = OpenOptions::new()
        .read(true)
        .open_loc(location)?
        .into_file()?;
    if file.read_at(page, ((entry.slot() + 1) * PAGE) as u64)? != PAGE {
        return Err(AxError::Io);
    }
    Ok(())
}

/// Restores a page and consumes one ownership reference.  Fault handling uses
/// `read` plus a replacement PTE so a failed map never loses swap state.
pub fn pagein(entry: SwapPte, page: &mut [u8]) -> AxResult<()> {
    read(entry, page)?;
    release(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_gate_linearizes_writers_mappings_and_activation() {
        let state = Arc::new(Mutex::new(MutationState {
            active: false,
            writers: 0,
            writable_mappings: 0,
        }));
        let writer = admit_mutation_state(state.clone()).unwrap();
        assert!(
            matches!(admit_activation_state(state.clone()), Err(error) if error == LinuxError::EBUSY.into())
        );
        drop(writer);
        let activation = admit_activation_state(state.clone()).unwrap();
        assert!(
            matches!(admit_mutation_state(state.clone()), Err(error) if error == LinuxError::EBUSY.into())
        );
        drop(activation);
        assert!(admit_mutation_state(state).is_ok());
    }

    #[test]
    fn swap_pte_round_trips_area_and_slot_without_present_pfn_bits() {
        let entry = SwapPte::new(0x1234, 0x1234_5678_9abc).unwrap();
        assert_eq!(entry.area(), 0x1234);
        assert_eq!(entry.slot(), 0x1234_5678_9abc);
        assert_ne!(entry.raw() & SwapPte::TAG, 0);
    }
}
