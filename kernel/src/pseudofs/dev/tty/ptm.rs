use alloc::sync::Arc;
use core::any::Any;

use axerrno::AxResult;
use axfs_ng_vfs::{DeviceId, NodeType};

use crate::{
    file::IoctlContext,
    pseudofs::{Device, DeviceOps, SimpleFs},
};

pub struct Ptmx(pub Arc<SimpleFs>);
impl Ptmx {
    pub fn create_pty(&self) -> AxResult<(Arc<Device>, Arc<super::PtyDriver>, u32)> {
        // Admission precedes worker construction. A full devpts table therefore
        // cannot create and immediately tear down an external reader task.
        let lease = super::pts::reserve_slave()?;
        let (master, slave) = super::pty::create_pty_pair()?;
        super::pts::add_slave(self.0.clone(), slave, &lease)?;
        let pty_number = master.pty_number();
        master.install_pts_lease(lease)?;
        let device = Device::try_new(
            self.0.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(128, pty_number),
            master.clone(),
        )?;
        Ok((device, master, pty_number))
    }
}

// This is implemented as null-ops since opening `Ptmx` would result in a new
// tty file and these implementations wouldn't actually be used
impl DeviceOps for Ptmx {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> AxResult<usize> {
        Err(axerrno::AxError::NotATty)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> AxResult<usize> {
        Err(axerrno::AxError::NotATty)
    }

    fn ioctl(&self, _context: &IoctlContext, _cmd: u32, _arg: usize) -> AxResult<usize> {
        Err(axerrno::AxError::NotATty)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
