use alloc::{borrow::Cow, string::String, sync::Arc, vec::Vec};
use core::{fmt::Write as _, sync::atomic::Ordering};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    DeviceId, Filesystem, FsName, FsNameBuf, MetadataUpdate, NodeOps, NodePermission, NodeType,
    VfsResult,
};
use bitmaps::{Bits, BitsImpl};
use flatten_objects::FlattenObjects;
use kspin::SpinNoIrq;

use crate::pseudofs::{
    ChildNames, Device, NodeOpsMux, SimpleDirOps, SimpleFs, dev::tty::pty::PtyDriver,
    try_boxed_names,
};

const PTS_CAPACITY: usize = 16;

/// A fixed-capacity reservation table. `None` is a private reserved slot and
/// `Some` is an atomically published devpts entry. It never allocates while
/// holding its IRQ-safe lock.
pub(super) struct SlotTable<T: ?Sized, const CAP: usize>
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

    fn reserve(self: &Arc<Self>) -> AxResult<SlotLease<T, CAP>> {
        let id = self
            .slots
            .lock()
            .add(None)
            .map_err(|_| AxError::StorageFull)?;
        Ok(SlotLease {
            table: self.clone(),
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

pub(super) struct SlotLease<T: ?Sized, const CAP: usize>
where
    BitsImpl<CAP>: Bits,
{
    table: Arc<SlotTable<T, CAP>>,
    id: usize,
    active: bool,
}

impl<T: ?Sized, const CAP: usize> SlotLease<T, CAP>
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

impl<T: ?Sized, const CAP: usize> Drop for SlotLease<T, CAP>
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

pub(super) type PtsLease = SlotLease<Device, PTS_CAPACITY>;
pub(super) type PtsTable = SlotTable<Device, PTS_CAPACITY>;

#[derive(Clone, Copy, Debug)]
pub(crate) struct DevPtsOptions {
    uid: Option<u32>,
    gid: Option<u32>,
    mode: u16,
    pub(super) ptmxmode: u16,
    root_uid: u32,
    root_gid: u32,
}

impl DevPtsOptions {
    pub(crate) const fn boot() -> Self {
        Self {
            uid: None,
            gid: Some(5),
            mode: 0o620,
            ptmxmode: 0o666,
            root_uid: 0,
            root_gid: 0,
        }
    }

    pub(crate) fn parse(data: &str, ns: &crate::task::UserNamespace) -> AxResult<Self> {
        let mut options = Self {
            uid: None,
            gid: None,
            mode: 0o600,
            ptmxmode: 0,
            root_uid: ns.make_kuid(0).ok_or(AxError::InvalidInput)?.into_raw(),
            root_gid: ns.make_kgid(0).ok_or(AxError::InvalidInput)?.into_raw(),
        };
        for option in data.split(',').filter(|option| !option.is_empty()) {
            if option == "newinstance" {
                continue;
            }
            let (key, value) = option.split_once('=').ok_or(AxError::InvalidInput)?;
            match key {
                "uid" => {
                    options.uid = Some(
                        ns.make_kuid(value.parse().map_err(|_| AxError::InvalidInput)?)
                            .ok_or(AxError::InvalidInput)?
                            .into_raw(),
                    )
                }
                "gid" => {
                    options.gid = Some(
                        ns.make_kgid(value.parse().map_err(|_| AxError::InvalidInput)?)
                            .ok_or(AxError::InvalidInput)?
                            .into_raw(),
                    )
                }
                "mode" | "ptmxmode" => {
                    let mode = u32::from_str_radix(value, 8).map_err(|_| AxError::InvalidInput)?;
                    let mode = (mode & 0o777) as u16;
                    if key == "mode" {
                        options.mode = mode;
                    } else {
                        options.ptmxmode = mode;
                    }
                }
                _ => return Err(AxError::InvalidInput),
            }
        }
        Ok(options)
    }
}

pub(crate) fn new_devpts(options: DevPtsOptions) -> AxResult<Filesystem> {
    let table = Arc::try_new(PtsTable::new()).map_err(|_| AxError::NoMemory)?;
    let fs = SimpleFs::new_with("devpts".into(), 0x1cd1, |fs| {
        let ptmx = Device::new_with_permissions(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 2),
            NodePermission::from_bits_truncate(options.ptmxmode),
            Arc::new(super::ptm::Ptmx {
                fs: fs.clone(),
                table: table.clone(),
                options,
            }),
        );
        ptmx.update_metadata(MetadataUpdate {
            owner: Some((options.root_uid, options.root_gid)),
            ..Default::default()
        })
        .expect("new devpts ptmx metadata");
        crate::pseudofs::SimpleDir::new_maker(fs, Arc::new(PtsDir { table, ptmx }))
    });
    fs.root_dir().update_metadata(MetadataUpdate {
        owner: Some((options.root_uid, options.root_gid)),
        ..Default::default()
    })?;
    Ok(fs)
}

pub(super) fn reserve_slave(table: &Arc<PtsTable>) -> AxResult<PtsLease> {
    table.reserve()
}

#[cfg(test)]
pub(super) fn reserve_test_lease() -> AxResult<(PtsLease, Arc<PtsTable>, usize)> {
    let table = Arc::try_new(PtsTable::new()).map_err(|_| AxError::NoMemory)?;
    let lease = table.reserve()?;
    let id = lease.id();
    Ok((lease, table, id))
}

#[cfg(test)]
pub(super) fn test_slot_reserved(table: &PtsTable, id: usize) -> bool {
    table.is_reserved(id)
}

pub(super) fn add_slave(
    fs: Arc<SimpleFs>,
    pty: Arc<PtyDriver>,
    lease: &PtsLease,
    options: DevPtsOptions,
    owner: (u32, u32),
    location: &axfs_ng_vfs::Location,
) -> AxResult<()> {
    // The caller reserves the numeric identity before constructing the PTY,
    // including its external line-discipline worker. The borrowed lease keeps
    // every publication failure rollback-safe without transferring ownership.
    let pty_number = lease.id() as u32;
    let terminal = pty.terminal.clone();
    let device = Device::try_new(
        fs,
        NodeType::CharacterDevice,
        DeviceId::new(136, pty_number),
        pty.clone(),
    )?;
    device.update_metadata(MetadataUpdate {
        owner: Some((
            options.uid.unwrap_or(owner.0),
            options.gid.unwrap_or(owner.1),
        )),
        mode: Some(NodePermission::from_bits_truncate(options.mode)),
        ..Default::default()
    })?;
    pty.remember_pts_location(location, &device);
    terminal.pty_number.store(pty_number, Ordering::Release);
    lease.publish(device).map_err(|device| {
        drop(device);
        AxError::BadState
    })?;
    Ok(())
}

/// /dev/pts directory
pub(super) struct PtsDir {
    table: Arc<PtsTable>,
    ptmx: Arc<Device>,
}

impl SimpleDirOps for PtsDir {
    fn is_cacheable(&self) -> bool {
        // Numeric slots are reused after the master closes. A cached dentry
        // would keep resolving the previous, permanently hung-up slave.
        false
    }

    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let snapshot = self.table.assigned_ids();
        let count = snapshot.iter().flatten().count() + 1;
        let mut names = Vec::new();
        names
            .try_reserve_exact(count)
            .map_err(|_| AxError::NoMemory)?;
        names.push(Cow::Borrowed(FsName::new(b"ptmx")));
        for id in snapshot.into_iter().flatten() {
            let mut name = String::new();
            name.try_reserve_exact(20).map_err(|_| AxError::NoMemory)?;
            write!(&mut name, "{id}").map_err(|_| AxError::NoMemory)?;
            names.push(Cow::Owned(FsNameBuf::from_vec(name.into_bytes())?));
        }
        try_boxed_names(names.into_iter())
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        if name.as_bytes() == b"ptmx" {
            return Ok(self.ptmx.clone().into());
        }
        let id = parse_decimal_name(name.as_bytes()).ok_or(AxError::NotFound)?;
        let pty = self.table.lookup(id).ok_or(AxError::NotFound)?;
        Ok(NodeOpsMux::File(pty))
    }
}

fn parse_decimal_name(bytes: &[u8]) -> Option<usize> {
    (!bytes.is_empty()).then_some(())?;
    bytes.iter().try_fold(0usize, |value, byte| {
        byte.checked_sub(b'0')
            .filter(|digit| *digit < 10)
            .and_then(|digit| value.checked_mul(10)?.checked_add(usize::from(digit)))
    })
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;

    #[derive(Debug)]
    struct Marker(u32);

    #[test]
    fn devpts_instances_keep_slots_and_ptmx_permissions_separate() {
        let _context = crate::test_support::scheduler_test_context();
        let first = new_devpts(DevPtsOptions::boot()).unwrap();
        let second = new_devpts(DevPtsOptions::boot()).unwrap();
        let first_mount = axfs_ng_vfs::Mountpoint::new_root(&first);
        let first_root = first_mount.root_location().entry().clone();
        let second_root = second.root_dir();
        let a = first_root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"ptmx"))
            .unwrap();
        let b = second_root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"ptmx"))
            .unwrap();
        assert_eq!(a.metadata().unwrap().mode.bits(), 0o666);
        assert_eq!(first.stat().unwrap().fs_type, 0x1cd1);
        let a = a.downcast::<Device>().unwrap();
        let b = b.downcast::<Device>().unwrap();
        let a = a
            .inner()
            .as_any()
            .downcast_ref::<super::super::ptm::Ptmx>()
            .unwrap();
        let b = b
            .inner()
            .as_any()
            .downcast_ref::<super::super::ptm::Ptmx>()
            .unwrap();
        assert!(!Arc::ptr_eq(&a.table, &b.table));
        let lease = reserve_slave(&a.table).unwrap();
        let other = reserve_slave(&b.table).unwrap();
        assert_eq!(lease.id(), 0);
        assert_eq!(other.id(), 0);
        let (_master, slave) = super::super::pty::create_pty_pair_for_test().unwrap();
        add_slave(
            a.fs.clone(),
            slave.clone(),
            &lease,
            a.options,
            (1234, 5678),
            &first_mount.root_location(),
        )
        .unwrap();
        let node = first_root
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"0"))
            .unwrap();
        let meta = node.metadata().unwrap();
        assert_eq!((meta.uid, meta.gid, meta.mode.bits()), (1234, 5, 0o620));
        let device = node.downcast::<Device>().unwrap();
        let resolved = slave.pts_location().unwrap();
        assert!(Arc::ptr_eq(resolved.mountpoint(), &first_mount));
        assert!(Arc::ptr_eq(
            &resolved.entry().downcast::<Device>().unwrap(),
            &device
        ));
        assert!(matches!(
            second_root.as_dir().unwrap().lookup(FsName::new(b"0")),
            Err(AxError::NotFound)
        ));
        drop(lease);
        assert!(!a.table.is_reserved(0));
        assert!(b.table.is_reserved(0));
        drop(other);
    }

    #[test]
    fn devpts_options_map_ids_and_reject_unmapped_or_malformed_values() {
        use thekernel_linux_cred::{IdMapInputExtent, Kgid, Kuid};
        let _context = crate::test_support::scheduler_test_context();
        let ns = crate::task::UserNamespace::try_new_root().unwrap();
        let bwrap = DevPtsOptions::parse("newinstance,ptmxmode=0666,mode=620", &ns).unwrap();
        assert_eq!(bwrap.mode, 0o620);
        assert_eq!(bwrap.ptmxmode, 0o666);
        assert_eq!(bwrap.gid, None);
        assert_eq!(
            DevPtsOptions::parse("gid=5,mode=620", &ns).unwrap().gid,
            Some(5)
        );
        for value in [
            "invalid",
            "mode=888",
            "gid=-1",
            "uid=4294967295",
            "ptmxmode=",
            "newinstance=1",
        ] {
            assert!(matches!(
                DevPtsOptions::parse(value, &ns),
                Err(AxError::InvalidInput)
            ));
        }
        let child = ns
            .try_fork(
                Kuid::from_raw(1000).unwrap(),
                Kgid::from_raw(2000).unwrap(),
                false,
            )
            .unwrap();
        child
            .publish_uid_map(
                child
                    .try_build_uid_map(alloc::vec![IdMapInputExtent::new(0, 1000, 1)])
                    .unwrap(),
            )
            .unwrap();
        child
            .publish_gid_map(
                child
                    .try_build_gid_map(alloc::vec![IdMapInputExtent::new(0, 2000, 1)])
                    .unwrap(),
                false,
            )
            .unwrap();
        let options = DevPtsOptions::parse("uid=0,gid=0,ptmxmode=666", &child).unwrap();
        assert_eq!(options.uid, Some(1000));
        assert_eq!(options.gid, Some(2000));
        assert!(matches!(
            DevPtsOptions::parse("gid=5", &child),
            Err(AxError::InvalidInput)
        ));
        let fs = new_devpts(options).unwrap();
        let ptmx = fs
            .root_dir()
            .as_dir()
            .unwrap()
            .lookup(FsName::new(b"ptmx"))
            .unwrap();
        let meta = ptmx.metadata().unwrap();
        assert_eq!((meta.uid, meta.gid), (1000, 2000));
    }

    #[test]
    fn vfs_lookup_revalidates_a_reused_slave_number() {
        let _context = crate::test_support::scheduler_test_context();
        let filesystem = new_devpts(DevPtsOptions::boot()).unwrap();
        let root = filesystem.root_dir();
        let dir = root.as_dir().unwrap();
        let ptmx = dir.lookup(FsName::new(b"ptmx")).unwrap();
        let ptmx = ptmx.downcast::<Device>().unwrap();
        let ptmx = ptmx
            .inner()
            .as_any()
            .downcast_ref::<super::super::ptm::Ptmx>()
            .unwrap();
        let table = ptmx.table.clone();
        let fs = ptmx.fs.clone();
        let lease = reserve_slave(&table).unwrap();
        let slot = lease.id();
        let name = alloc::format!("{slot}");
        let (_master, slave) = super::super::pty::create_pty_pair_for_test().unwrap();
        let old = Device::try_new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(136, slot as u32),
            slave,
        )
        .unwrap();
        assert!(lease.publish(old).is_ok());
        let old_entry = dir.lookup(FsName::new(name.as_bytes())).unwrap();
        drop(lease);

        let lease = reserve_slave(&table).unwrap();
        assert_eq!(lease.id(), slot);
        let (_master, slave) = super::super::pty::create_pty_pair_for_test().unwrap();
        let new = Device::try_new(
            fs,
            NodeType::CharacterDevice,
            DeviceId::new(136, slot as u32),
            slave,
        )
        .unwrap();
        assert!(lease.publish(new).is_ok());
        let new_entry = dir.lookup(FsName::new(name.as_bytes())).unwrap();
        assert_ne!(old_entry.inode(), new_entry.inode());
    }

    #[test]
    fn lease_reuses_index_beyond_capacity_and_open_clone_survives_unlink() {
        let table = Arc::new(SlotTable::<Marker, PTS_CAPACITY>::new());
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
    fn lease_keeps_an_unmounted_instance_table_alive_until_last_close() {
        let table = Arc::new(SlotTable::<Marker, PTS_CAPACITY>::new());
        let weak = Arc::downgrade(&table);
        let lease = table.reserve().unwrap();
        drop(table);
        assert!(weak.upgrade().is_some());
        drop(lease);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn reservation_limit_is_honest_and_all_slots_roll_back() {
        let table = Arc::new(SlotTable::<Marker, PTS_CAPACITY>::new());
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
