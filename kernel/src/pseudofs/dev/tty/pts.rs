use alloc::{borrow::Cow, string::ToString, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{DeviceId, MetadataUpdate, NodeOps, NodePermission, NodeType, VfsResult};
use axtask::current;
use flatten_objects::FlattenObjects;
use kspin::SpinNoIrq;

use crate::{
    pseudofs::{
        ChildNames, Device, NodeOpsMux, SimpleDirOps, SimpleFs, dev::tty::pty::PtyDriver,
        try_boxed_names,
    },
    task::AsThread,
};

static PTS_TABLE: SpinNoIrq<FlattenObjects<Arc<Device>, 16>> =
    SpinNoIrq::new(FlattenObjects::new());

pub fn add_slave(fs: Arc<SimpleFs>, pty: Arc<PtyDriver>) -> AxResult<u32> {
    let terminal = pty.terminal.clone();
    let mut table = PTS_TABLE.lock();
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let device = Device::new(fs, NodeType::CharacterDevice, DeviceId::default(), pty);
    device.update_metadata(MetadataUpdate {
        owner: Some((proc_data.uid(), proc_data.gid())),
        mode: Some(NodePermission::from_bits_truncate(0o620)),
        ..Default::default()
    })?;
    let pty_number = table.add(device).map_err(|_| AxError::TooManyOpenFiles)? as u32;
    terminal.pty_number.store(pty_number, Ordering::Release);
    table
        .get(pty_number as usize)
        .unwrap()
        .set_device_id(DeviceId::new(136, pty_number));
    Ok(pty_number)
}

/// /dev/pts directory
pub struct PtsDir;

impl SimpleDirOps for PtsDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let ids = PTS_TABLE
            .lock()
            .ids()
            .map(|it| Cow::Owned(it.to_string()))
            .collect::<Vec<_>>();
        try_boxed_names(ids.into_iter())
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        let id = name.parse::<usize>().map_err(|_| AxError::InvalidData)?;
        let pty = PTS_TABLE.lock().get(id).ok_or(AxError::NotFound)?.clone();
        Ok(NodeOpsMux::File(pty))
    }
}
