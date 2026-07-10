use alloc::vec::Vec;
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

const INLINE_IOVEC_CAPACITY: usize = 8;
const EMPTY_IOVEC: IoVec = IoVec {
    iov_base: core::ptr::null_mut(),
    iov_len: 0,
};

enum ImportedIoVecs {
    Inline {
        entries: [IoVec; INLINE_IOVEC_CAPACITY],
        len: usize,
    },
    Heap(Vec<IoVec>),
}

impl ImportedIoVecs {
    fn with_capacity(len: usize) -> AxResult<Self> {
        if len <= INLINE_IOVEC_CAPACITY {
            Ok(Self::Inline {
                entries: [EMPTY_IOVEC; INLINE_IOVEC_CAPACITY],
                len: 0,
            })
        } else {
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(len)
                .map_err(|_| AxError::NoMemory)?;
            Ok(Self::Heap(entries))
        }
    }

    fn push(&mut self, entry: IoVec) {
        match self {
            Self::Inline { entries, len } => {
                entries[*len] = entry;
                *len += 1;
            }
            Self::Heap(entries) => entries.push(entry),
        }
    }

    fn as_slice(&self) -> &[IoVec] {
        match self {
            Self::Inline { entries, len } => &entries[..*len],
            Self::Heap(entries) => entries,
        }
    }
}

pub struct IoVectorBuf {
    iovs: ImportedIoVecs,
    len: usize,
}

impl IoVectorBuf {
    pub fn new(iovs: *const IoVec, iovcnt: usize) -> AxResult<Self> {
        if iovcnt > 1024 {
            return Err(AxError::InvalidInput);
        }
        if iovcnt > 0 {
            let bytes = iovcnt
                .checked_mul(mem::size_of::<IoVec>())
                .ok_or(AxError::BadAddress)?;
            check_user_readable(iovs as usize, bytes)?;
        }
        let mut imported = ImportedIoVecs::with_capacity(iovcnt)?;
        let mut len: usize = 0;
        for i in 0..iovcnt {
            let iov = iovs.wrapping_add(i).vm_read()?;
            if iov.iov_len < 0 {
                return Err(AxError::InvalidInput);
            }
            if len < MAX_RW_COUNT {
                len += (iov.iov_len as usize).min(MAX_RW_COUNT - len);
            }
            imported.push(iov);
        }
        Ok(Self {
            iovs: imported,
            len,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn iovcnt(&self) -> usize {
        self.iovs.as_slice().len()
    }

    pub fn entry(&self, index: usize) -> AxResult<IoVec> {
        self.iovs
            .as_slice()
            .get(index)
            .copied()
            .ok_or(AxError::InvalidInput)
    }

    pub fn is_aligned(&self, align: usize) -> AxResult<bool> {
        for iov in self.iovs.as_slice() {
            let len = iov.iov_len as usize;
            if len == 0 {
                continue;
            }
            if !(iov.iov_base as usize).is_multiple_of(align) || !len.is_multiple_of(align) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn check_readable(&self) -> AxResult<()> {
        for iov in self.iovs.as_slice() {
            let len = iov.iov_len as usize;
            if len == 0 {
                continue;
            }
            check_user_readable(iov.iov_base as usize, len)?;
        }
        Ok(())
    }

    pub fn check_writable(&self) -> AxResult<()> {
        for iov in self.iovs.as_slice() {
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
        while self.start < self.inner.iovs.as_slice().len() {
            let iov = self.inner.iovs.as_slice()[self.start];
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
            if self.start >= self.inner.iovs.as_slice().len() {
                break;
            }
            let iov = self.inner.iovs.as_slice()[self.start];
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
            if self.start >= self.inner.iovs.as_slice().len() {
                break;
            }
            let iov = self.inner.iovs.as_slice()[self.start];
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
