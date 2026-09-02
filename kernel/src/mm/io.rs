use alloc::vec::Vec;
use core::mem::{self, MaybeUninit};

use axerrno::{AxError, AxResult};
use axio::prelude::*;
pub use thekernel_linux_mm::IoVec;

use super::{
    UserMemoryCapability, check_user_readable_with, check_user_writable_with, map_usercopy_error,
};

const MAX_RW_COUNT: usize = 0x7fff_f000;

const INLINE_IOVEC_CAPACITY: usize = 8;
const EMPTY_IOVEC: IoVec = IoVec {
    iov_base: 0,
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
    capability: UserMemoryCapability,
    iovs: ImportedIoVecs,
    len: usize,
}

impl IoVectorBuf {
    pub fn new(
        capability: UserMemoryCapability,
        iovs: *const IoVec,
        iovcnt: usize,
    ) -> AxResult<Self> {
        if iovcnt > 1024 {
            return Err(AxError::InvalidInput);
        }
        if iovcnt > 0 {
            let bytes = iovcnt
                .checked_mul(mem::size_of::<IoVec>())
                .ok_or(AxError::BadAddress)?;
            check_user_readable_with(&capability, iovs as usize, bytes)?;
        }
        let mut imported = ImportedIoVecs::with_capacity(iovcnt)?;
        let mut len: usize = 0;
        for i in 0..iovcnt {
            let iov = capability
                .read_value(iovs.wrapping_add(i))
                .map_err(map_usercopy_error)?;
            if iov.iov_len < 0 {
                return Err(AxError::InvalidInput);
            }
            if len < MAX_RW_COUNT {
                len += (iov.iov_len as usize).min(MAX_RW_COUNT - len);
            }
            imported.push(iov);
        }
        Ok(Self {
            capability,
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

    /// Returns the longest leading byte count whose contributing iovecs all
    /// satisfy `align`. Any aligned prefix no longer than this value is valid.
    pub fn aligned_prefix_len(&self, align: usize) -> AxResult<usize> {
        if align == 0 {
            return Err(AxError::InvalidInput);
        }

        let mut remaining = self.len;
        let mut aligned = 0usize;
        for iov in self.iovs.as_slice() {
            if remaining == 0 {
                break;
            }
            let chunk = (iov.iov_len as usize).min(remaining);
            if chunk == 0 {
                continue;
            }
            if !(iov.iov_base as usize).is_multiple_of(align) {
                break;
            }
            let aligned_chunk = chunk - chunk % align;
            aligned += aligned_chunk;
            if aligned_chunk != chunk {
                break;
            }
            remaining -= chunk;
        }
        Ok(aligned)
    }

    pub fn check_readable(&self) -> AxResult<()> {
        for iov in self.iovs.as_slice() {
            let len = iov.iov_len as usize;
            if len == 0 {
                continue;
            }
            check_user_readable_with(&self.capability, iov.iov_base as usize, len)?;
        }
        Ok(())
    }

    pub fn check_writable(&self) -> AxResult<()> {
        for iov in self.iovs.as_slice() {
            let len = iov.iov_len as usize;
            if len == 0 {
                continue;
            }
            check_user_writable_with(&self.capability, iov.iov_base as usize, len)?;
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

    pub fn capability(&self) -> &UserMemoryCapability {
        &self.capability
    }
}

pub struct IoVectorBufIo {
    inner: IoVectorBuf,
    start: usize,
    offset: usize,
}

impl IoVectorBufIo {
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
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    buf[count..count + len]
                        .as_mut_ptr()
                        .cast::<MaybeUninit<u8>>(),
                    len,
                )
            };
            if let Err(error) = self
                .inner
                .capability
                .read_slice(
                    (iov.iov_base as usize).wrapping_add(self.offset) as *const u8,
                    dst,
                )
                .map_err(map_usercopy_error)
            {
                // A successful prefix is observable progress for axio's
                // read contract. Keep the failed iovec untouched so the
                // caller can retry it on the next call.
                return if count != 0 { Ok(count) } else { Err(error) };
            }
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
            if let Err(error) = self
                .inner
                .capability
                .write_bytes(
                    (iov.iov_base as usize).wrapping_add(self.offset),
                    &buf[count..count + len],
                )
                .map_err(map_usercopy_error)
            {
                // As with Read, report a completed prefix and leave the
                // faulting iovec's offset/remaining state unchanged.
                return if count != 0 { Ok(count) } else { Err(error) };
            }
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

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axhal::paging::{MappingFlags, PageSize};
    use axsync::Mutex;
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::*;

    #[repr(align(512))]
    struct Aligned([u8; 2048]);

    fn test_capability() -> UserMemoryCapability {
        UserMemoryCapability::new(Arc::new(Mutex::new(
            super::super::AddrSpace::new_empty(VirtAddr::from(0x1000), 0x1000).unwrap(),
        )))
    }

    fn imported_iov(entries: &[IoVec], len: usize) -> IoVectorBuf {
        let mut imported = ImportedIoVecs::with_capacity(entries.len()).unwrap();
        for entry in entries {
            imported.push(*entry);
        }
        IoVectorBuf {
            capability: test_capability(),
            iovs: imported,
            len,
        }
    }

    fn mapped_capability() -> UserMemoryCapability {
        let mut address_space =
            super::super::AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K * 4).unwrap();
        for base in [0x1000, 0x3000] {
            address_space
                .map(
                    VirtAddr::from(base),
                    PAGE_SIZE_4K,
                    MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                    false,
                    super::super::Backend::new_alloc(VirtAddr::from(base), PageSize::Size4K),
                )
                .unwrap();
        }
        UserMemoryCapability::new(Arc::new(Mutex::new(address_space)))
    }

    #[test]
    fn iovec_descriptor_and_payload_boundaries_use_the_capability() {
        let capability = mapped_capability();
        let descriptor = IoVec {
            iov_base: 0x3000,
            iov_len: PAGE_SIZE_4K as i64,
        };
        // The descriptor page is mapped, and the payload page is mapped. The
        // constructor must import both through the selected address space.
        unsafe {
            capability
                .write_value_unchecked(0x1000 as *mut IoVec, descriptor)
                .unwrap();
        }
        let imported = IoVectorBuf::new(capability.clone(), 0x1000 as *const IoVec, 1).unwrap();
        assert_eq!(imported.entry(0).unwrap().iov_len, PAGE_SIZE_4K as i64);
        imported.check_readable().unwrap();

        // The descriptor itself crosses from the mapped 0x1000 page into an
        // unmapped page, so it must fail before any payload validation.
        assert!(matches!(
            IoVectorBuf::new(capability.clone(), 0x1ff8 as *const IoVec, 1),
            Err(AxError::BadAddress)
        ));

        // The descriptor is valid, but a payload one byte beyond the mapped
        // page must be rejected by the separate payload check.
        let crossing = IoVec {
            iov_base: 0x3000,
            iov_len: PAGE_SIZE_4K as i64 + 1,
        };
        unsafe {
            capability
                .write_value_unchecked(0x1000 as *mut IoVec, crossing)
                .unwrap();
        }
        let imported = IoVectorBuf::new(capability, 0x1000 as *const IoVec, 1).unwrap();
        assert!(matches!(
            imported.check_readable(),
            Err(AxError::BadAddress)
        ));
    }

    fn imported_iov_with_capability(
        capability: UserMemoryCapability,
        entries: &[IoVec],
    ) -> IoVectorBuf {
        let mut imported = ImportedIoVecs::with_capacity(entries.len()).unwrap();
        let mut len = 0;
        for entry in entries {
            imported.push(*entry);
            len += entry.iov_len as usize;
        }
        IoVectorBuf {
            capability,
            iovs: imported,
            len,
        }
    }

    fn map_page(capability: &UserMemoryCapability, base: usize) {
        capability
            .address_space()
            .lock()
            .map(
                VirtAddr::from(base),
                PAGE_SIZE_4K,
                MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                false,
                super::super::Backend::new_alloc(VirtAddr::from(base), PageSize::Size4K),
            )
            .unwrap();
    }

    #[test]
    fn read_reports_prefix_and_retries_the_failed_iovec() {
        let capability = mapped_capability();
        capability.write_bytes(0x1000, &[0x11]).unwrap();
        let mut io = imported_iov_with_capability(
            capability.clone(),
            &[
                IoVec {
                    iov_base: 0x1000,
                    iov_len: 1,
                },
                IoVec {
                    iov_base: 0x2000,
                    iov_len: 1,
                },
            ],
        )
        .into_io();

        let mut output = [0u8; 2];
        assert_eq!(io.read(&mut output), Ok(1));
        assert_eq!(output[0], 0x11);
        assert_eq!(io.remaining(), 1);

        // The failed second iovec is not consumed, so a retry still reports
        // its original usercopy error and leaves the state unchanged.
        let mut retry = [0u8; 1];
        assert_eq!(io.read(&mut retry), Err(AxError::BadAddress));
        assert_eq!(io.remaining(), 1);

        map_page(&capability, 0x2000);
        capability.write_bytes(0x2000, &[0x22]).unwrap();
        assert_eq!(io.read(&mut retry), Ok(1));
        assert_eq!(retry[0], 0x22);
        assert_eq!(io.remaining(), 0);
    }

    #[test]
    fn write_reports_prefix_and_retries_the_failed_iovec() {
        let capability = mapped_capability();
        let mut io = imported_iov_with_capability(
            capability.clone(),
            &[
                IoVec {
                    iov_base: 0x1000,
                    iov_len: 1,
                },
                IoVec {
                    iov_base: 0x2000,
                    iov_len: 1,
                },
            ],
        )
        .into_io();

        assert_eq!(io.write(&[0x31, 0x32]), Ok(1));
        assert_eq!(io.remaining(), 1);
        let mut first = [MaybeUninit::<u8>::uninit()];
        capability.read_bytes(0x1000, &mut first).unwrap();
        // SAFETY: the explicit capability read initialized the byte.
        assert_eq!(unsafe { first[0].assume_init() }, 0x31);

        assert_eq!(io.write(&[0x32]), Err(AxError::BadAddress));
        assert_eq!(io.remaining(), 1);

        map_page(&capability, 0x2000);
        assert_eq!(io.write(&[0x32]), Ok(1));
        assert_eq!(io.remaining(), 0);
        let mut second = [MaybeUninit::<u8>::uninit()];
        capability.read_bytes(0x2000, &mut second).unwrap();
        // SAFETY: the explicit capability read initialized the byte.
        assert_eq!(unsafe { second[0].assume_init() }, 0x32);
    }

    #[test]
    fn first_iovec_fault_preserves_error_and_state() {
        let capability = mapped_capability();
        let entries = [IoVec {
            iov_base: 0x2000,
            iov_len: 1,
        }];

        let mut reader = imported_iov_with_capability(capability.clone(), &entries).into_io();
        let mut output = [0xa5u8];
        assert_eq!(reader.read(&mut output), Err(AxError::BadAddress));
        assert_eq!(output, [0xa5]);
        assert_eq!(reader.remaining(), 1);

        let mut writer = imported_iov_with_capability(capability, &entries).into_io();
        assert_eq!(writer.write(&[0x5a]), Err(AxError::BadAddress));
        assert_eq!(writer.remaining(), 1);
    }

    #[test]
    fn aligned_prefix_stops_before_a_bad_base_or_partial_sector() {
        let storage = Aligned([0; 2048]);
        let base = storage.0.as_ptr().cast_mut();

        let bad_base = imported_iov(
            &[
                IoVec {
                    iov_base: base as u64,
                    iov_len: 512,
                },
                IoVec {
                    iov_base: base.wrapping_add(513) as u64,
                    iov_len: 512,
                },
            ],
            1024,
        );
        assert_eq!(bad_base.aligned_prefix_len(512), Ok(512));

        let partial_sector = imported_iov(
            &[
                IoVec {
                    iov_base: base as u64,
                    iov_len: 768,
                },
                IoVec {
                    iov_base: base.wrapping_add(1024) as u64,
                    iov_len: 512,
                },
            ],
            1280,
        );
        assert_eq!(partial_sector.aligned_prefix_len(512), Ok(512));

        let aligned = imported_iov(
            &[
                IoVec {
                    iov_base: base as u64,
                    iov_len: 512,
                },
                IoVec {
                    iov_base: base.wrapping_add(1024) as u64,
                    iov_len: 512,
                },
            ],
            1024,
        );
        assert_eq!(aligned.aligned_prefix_len(512), Ok(1024));
    }
}
