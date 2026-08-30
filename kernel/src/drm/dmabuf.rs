//! Generic PRIME dma-buf OFDs.  The OFD owns an `Arc<GemObject>`, so closing
//! either the exporting handle or dma-buf fd cannot invalidate mappings or a
//! scanout which already retained the object.

use alloc::{borrow::Cow, sync::Arc};
use core::task::Context;

use axerrno::AxResult;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};

use super::gem::GemObject;
use crate::file::{FileLike, Kstat};

pub struct DmaBufFile {
    object: Arc<GemObject>,
}
impl DmaBufFile {
    pub(crate) fn new(object: Arc<GemObject>) -> Arc<Self> {
        Arc::new(Self { object })
    }
    pub(crate) fn object(&self) -> Arc<GemObject> {
        self.object.clone()
    }
}
impl FileLike for DmaBufFile {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(crate::file::anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[dmabuf]".into())
    }
    fn set_nonblocking(&self, _: bool) -> AxResult<()> {
        Ok(())
    }
}
impl Pollable for DmaBufFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _: &mut Context<'_>,
        _: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        axpoll::PreparedPollRegistration::try_new(0)?.commit()
    }
}

pub(crate) fn export(
    object: Arc<GemObject>,
    context: &crate::file::IoctlContext,
    cloexec: bool,
) -> AxResult<i32> {
    context.add_file_like(DmaBufFile::new(object), cloexec)
}
pub(crate) fn import(context: &crate::file::IoctlContext, fd: i32) -> AxResult<Arc<GemObject>> {
    let file = context.get_file_like(fd)?;
    file.downcast::<DmaBufFile>().map(|buf| buf.object())
}
