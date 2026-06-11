use core::mem::{self, MaybeUninit};

use axerrno::{AxError, AxResult};
use axio::prelude::*;
use bytemuck::AnyBitPattern;
use starry_vm::{VmPtr, vm_read_slice, vm_write_slice};

use super::{check_user_readable, check_user_writable};

const MAX_RW_COUNT: usize = 0x7fff_f000;

#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct IoVec {
    pub iov_base: *mut u8,
    pub iov_len: isize,
}

#[derive(Default)]
pub struct IoVectorBuf {
    iovs: *const IoVec,
    iovcnt: usize,
    len: usize,
}

impl IoVectorBuf {
    pub fn new(iovs: *const IoVec, iovcnt: usize) -> AxResult<Self> {
        if iovcnt > 1024 {
            return Err(AxError::InvalidInput);
        }
        let mut len: usize = 0;
        for i in 0..iovcnt {
            let iov = iovs.wrapping_add(i).vm_read()?;
            if iov.iov_len < 0 {
                return Err(AxError::InvalidInput);
            }
            if len < MAX_RW_COUNT {
                len += (iov.iov_len as usize).min(MAX_RW_COUNT - len);
            }
        }
        Ok(Self { iovs, iovcnt, len })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn check_readable(&self) -> AxResult<()> {
        for i in 0..self.iovcnt {
            let iov = self.iovs.wrapping_add(i).vm_read()?;
            if iov.iov_len < 0 {
                return Err(AxError::InvalidInput);
            }
            let len = iov.iov_len as usize;
            if len == 0 {
                continue;
            }
            check_user_readable(iov.iov_base as usize, len)?;
        }
        Ok(())
    }

    pub fn check_writable(&self) -> AxResult<()> {
        for i in 0..self.iovcnt {
            let iov = self.iovs.wrapping_add(i).vm_read()?;
            if iov.iov_len < 0 {
                return Err(AxError::InvalidInput);
            }
            let len = iov.iov_len as usize;
            if len == 0 {
                continue;
            }
            check_user_writable(iov.iov_base as usize, len)?;
        }
        Ok(())
    }

    pub fn into_io(self) -> IoVectorBufIo {
        IoVectorBufIo {
            inner: self,
            start: 0,
            offset: 0,
        }
    }
}

pub struct IoVectorBufIo {
    inner: IoVectorBuf,
    start: usize,
    offset: usize,
}

impl IoVectorBufIo {
    pub fn limit_remaining(&mut self, len: usize) {
        self.inner.len = self.inner.len.min(len);
    }

    fn skip_empty(&mut self) -> AxResult<()> {
        while self.start < self.inner.iovcnt {
            let iov = self.inner.iovs.wrapping_add(self.start).vm_read()?;
            if iov.iov_len as usize > self.offset {
                break;
            }
            self.offset = 0;
            self.start += 1;
        }
        Ok(())
    }
}

impl Read for IoVectorBufIo {
    fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
        let mut count = 0;
        loop {
            if self.inner.len == 0 {
                break;
            }
            self.skip_empty()?;
            if self.start >= self.inner.iovcnt {
                break;
            }
            let iov = self.inner.iovs.wrapping_add(self.start).vm_read()?;
            let len = (iov.iov_len as usize - self.offset)
                .min(buf.len() - count)
                .min(self.inner.len);
            if len == 0 {
                break;
            }
            vm_read_slice(iov.iov_base.wrapping_add(self.offset), unsafe {
                mem::transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut buf[count..count + len])
            })?;
            self.offset += len;
            self.inner.len -= len;
            count += len;
        }
        Ok(count)
    }
}

impl Write for IoVectorBufIo {
    fn write(&mut self, buf: &[u8]) -> AxResult<usize> {
        let mut count = 0;
        loop {
            if self.inner.len == 0 {
                break;
            }
            self.skip_empty()?;
            if self.start >= self.inner.iovcnt {
                break;
            }
            let iov = self.inner.iovs.wrapping_add(self.start).vm_read()?;
            let len = (iov.iov_len as usize - self.offset)
                .min(buf.len() - count)
                .min(self.inner.len);
            if len == 0 {
                break;
            }
            vm_write_slice(
                iov.iov_base.wrapping_add(self.offset),
                &buf[count..count + len],
            )?;
            self.offset += len;
            self.inner.len -= len;
            count += len;
        }
        Ok(count)
    }

    fn flush(&mut self) -> AxResult {
        Ok(())
    }
}

impl IoBuf for IoVectorBufIo {
    fn remaining(&self) -> usize {
        self.inner.len
    }
}

impl IoBufMut for IoVectorBufIo {
    fn remaining_mut(&self) -> usize {
        self.inner.len
    }
}
