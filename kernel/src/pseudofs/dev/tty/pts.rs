use alloc::{borrow::Cow, string::String, sync::Arc, vec::Vec};
use core::{fmt::Write as _, sync::atomic::Ordering};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{DeviceId, MetadataUpdate, NodeOps, NodePermission, NodeType, VfsResult};
use axtask::current;
use bitmaps::{Bits, BitsImpl};
use flatten_objects::FlattenObjects;
use kspin::SpinNoIrq;

use crate::{
    pseudofs::{
        ChildNames, Device, NodeOpsMux, SimpleDirOps, SimpleFs, dev::tty::pty::PtyDriver,
        try_boxed_names,
    },
    task::AsThread,
};

const PTS_CAPACITY: usize = 16;

/// A fixed-capacity reservation table. `None` is a private reserved slot and
/// `Some` is an atomically published devpts entry. It never allocates while
/// holding its IRQ-safe lock.
struct SlotTable<T: ?Sized, const CAP: usize>
where
    BitsImpl<CAP>: Bits,
{
    slots: SpinNoIrq<FlattenObjects<Option<Arc<T>>, CAP>>,
}

impl<T: ?Sized, const CAP: usize> SlotTable<T, CAP>
where
    BitsImpl<CAP>: Bits,
{
    const fn new() -> Self {
        Self {
            slots: SpinNoIrq::new(FlattenObjects::new()),
        }
    }

    fn reserve(&self) -> AxResult<SlotLease<'_, T, CAP>> {
        let id = self
            .slots
            .lock()
            .add(None)
            .map_err(|_| AxError::StorageFull)?;
        Ok(SlotLease {
            table: self,
            id,
            active: true,
        })
    }

    fn publish(&self, id: usize, value: Arc<T>) -> Result<(), Arc<T>> {
        {
            let mut slots = self.slots.lock();
            match slots.get_mut(id) {
                Some(slot) if slot.is_none() => {
                    *slot = Some(value);
                    Ok(())
                }
                _ => Err(value),
            }
        }
    }

    fn lookup(&self, id: usize) -> Option<Arc<T>> {
        self.slots.lock().get(id).and_then(Option::as_ref).cloned()
    }

    fn remove(&self, id: usize) {
        // Removing transfers the Arc out of the table. Its destructor runs
        // only after the spin guard has gone away.
        let removed = {
            let mut slots = self.slots.lock();
            slots.remove(id)
        };
        drop(removed);
    }

    fn assigned_ids(&self) -> [Option<usize>; CAP] {
        let mut snapshot = [None; CAP];
        let slots = self.slots.lock();
        for (dst, id) in snapshot.iter_mut().zip(slots.ids()) {
            if slots.get(id).is_some_and(Option::is_some) {
                *dst = Some(id);
            }
        }
        snapshot
    }

    #[cfg(test)]
    fn is_reserved(&self, id: usize) -> bool {
        self.slots.lock().get(id).is_some()
    }
}

pub(super) struct SlotLease<'a, T: ?Sized, const CAP: usize>
where
    BitsImpl<CAP>: Bits,
{
    table: &'a SlotTable<T, CAP>,
    id: usize,
    active: bool,
}

impl<T: ?Sized, const CAP: usize> SlotLease<'_, T, CAP>
where
    BitsImpl<CAP>: Bits,
{
    fn id(&self) -> usize {
        self.id
    }

    fn publish(&self, value: Arc<T>) -> Result<(), Arc<T>> {
        self.table.publish(self.id, value)
    }
}

impl<T: ?Sized, const CAP: usize> Drop for SlotLease<'_, T, CAP>
where
    BitsImpl<CAP>: Bits,
{
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.table.remove(self.id);
        }
    }
}

static PTS_TABLE: SlotTable<Device, PTS_CAPACITY> = SlotTable::new();

pub(super) type PtsLease = SlotLease<'static, Device, PTS_CAPACITY>;

pub(super) fn reserve_slave() -> AxResult<PtsLease> {
    PTS_TABLE.reserve()
}

#[cfg(test)]
pub(super) fn reserve_test_lease() -> AxResult<(PtsLease, usize)> {
    let lease = PTS_TABLE.reserve()?;
    let id = lease.id();
    Ok((lease, id))
}

#[cfg(test)]
pub(super) fn test_slot_reserved(id: usize) -> bool {
    PTS_TABLE.is_reserved(id)
}

pub fn add_slave(fs: Arc<SimpleFs>, pty: Arc<PtyDriver>, lease: &PtsLease) -> AxResult<()> {
    // The caller reserves the numeric identity before constructing the PTY,
    // including its external line-discipline worker. The borrowed lease keeps
    // every publication failure rollback-safe without transferring ownership.
    let pty_number = lease.id() as u32;
    let terminal = pty.terminal.clone();
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let device = Device::try_new(
        fs,
        NodeType::CharacterDevice,
        DeviceId::new(136, pty_number),
        pty,
    )?;
    device.update_metadata(MetadataUpdate {
        owner: Some((proc_data.uid(), proc_data.gid())),
        mode: Some(NodePermission::from_bits_truncate(0o620)),
        ..Default::default()
    })?;
    terminal.pty_number.store(pty_number, Ordering::Release);
    lease.publish(device).map_err(|device| {
        drop(device);
        AxError::BadState
    })?;
    Ok(())
}

/// /dev/pts directory
pub struct PtsDir;

impl SimpleDirOps for PtsDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let snapshot = PTS_TABLE.assigned_ids();
        let count = snapshot.iter().flatten().count();
        let mut names = Vec::new();
        names
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        for id in snapshot.into_iter().flatten() {
            let mut name = String::new();
            name.try_reserve_exact(20).map_err(|_| AxError::NoMemory)?;
            write!(&mut name, "{id}").map_err(|_| AxError::NoMemory)?;
            names.push(Cow::Owned(name));
        }
        try_boxed_names(names.into_iter())
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let id = name.parse::<usize>().map_err(|_| AxError::InvalidData)?;
        let pty = PTS_TABLE.lookup(id).ok_or(AxError::NotFound)?;
        Ok(NodeOpsMux::File(pty))
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;

    #[derive(Debug)]
    struct Marker(u32);

    #[test]
    fn lease_reuses_index_beyond_capacity_and_open_clone_survives_unlink() {
        let table = SlotTable::<Marker, PTS_CAPACITY>::new();
        for generation in 0..(PTS_CAPACITY * 2 + 3) {
            let lease = table.reserve().unwrap();
            assert_eq!(lease.id(), 0);
            let marker = Arc::try_new(Marker(generation as u32)).unwrap();
            lease.publish(marker).unwrap();
            let opened = table.lookup(0).unwrap();
            drop(lease);

            assert!(table.lookup(0).is_none());
            assert_eq!(opened.0, generation as u32);
        }
    }

    #[test]
    fn reservation_limit_is_honest_and_all_slots_roll_back() {
        let table = SlotTable::<Marker, PTS_CAPACITY>::new();
        let mut leases = Vec::new();
        leases.try_reserve_exact(PTS_CAPACITY).unwrap();
        for expected in 0..PTS_CAPACITY {
            let lease = table.reserve().unwrap();
            assert_eq!(lease.id(), expected);
            leases.push(lease);
        }
        assert!(matches!(table.reserve(), Err(AxError::StorageFull)));
        drop(leases);
        assert_eq!(table.reserve().unwrap().id(), 0);
    }
}
