//! Whole-image interpreter placement; no guessed maximum executable size.
use axerrno::{AxError, AxResult};
use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};
use xmas_elf::program::{ProgramHeader64, Type};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ImageEnvelope {
    start: usize,
    size: usize,
    align: usize,
}
impl ImageEnvelope {
    pub(super) fn from_headers(headers: &[ProgramHeader64]) -> AxResult<Self> {
        let mut start = usize::MAX;
        let mut end = 0;
        let mut align = PAGE_SIZE_4K;
        for header in headers
            .iter()
            .filter(|h| h.get_type() == Ok(Type::Load) && h.mem_size != 0)
        {
            let address =
                usize::try_from(header.virtual_addr).map_err(|_| AxError::InvalidExecutable)?;
            let size = usize::try_from(header.mem_size).map_err(|_| AxError::InvalidExecutable)?;
            start = start.min(address);
            end = end.max(
                address
                    .checked_add(size)
                    .ok_or(AxError::InvalidExecutable)?,
            );
            let candidate =
                usize::try_from(header.align).map_err(|_| AxError::InvalidExecutable)?;
            if candidate.is_power_of_two() {
                align = align.max(candidate);
            }
        }
        if start == usize::MAX {
            return Err(AxError::InvalidExecutable);
        }
        start &= !(align - 1);
        end = end
            .checked_add(align - 1)
            .ok_or(AxError::InvalidExecutable)?
            & !(align - 1);
        Ok(Self {
            start,
            size: end
                .checked_sub(start)
                .filter(|n| *n != 0)
                .ok_or(AxError::InvalidExecutable)?,
            align,
        })
    }
    pub(super) fn place(
        self,
        preferred_bias: usize,
        limit: VirtAddrRange,
        find: impl FnOnce(VirtAddr, usize, VirtAddrRange, usize) -> Option<VirtAddr>,
    ) -> AxResult<usize> {
        let hint = preferred_bias
            .checked_add(self.start)
            .ok_or(AxError::InvalidExecutable)?;
        let address = find(VirtAddr::from_usize(hint), self.size, limit, self.align)
            .ok_or(AxError::NoMemory)?;
        if address < limit.start
            || address.as_usize() % self.align != 0
            || address
                .as_usize()
                .checked_add(self.size)
                .is_none_or(|end| end > limit.end.as_usize())
        {
            return Err(AxError::NoMemory);
        }
        address
            .as_usize()
            .checked_sub(self.start)
            .ok_or(AxError::InvalidExecutable)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn load(address: usize, size: usize, align: usize) -> ProgramHeader64 {
        let mut header = ProgramHeader64 {
            type_: Default::default(),
            flags: xmas_elf::program::Flags(6),
            offset: 0,
            virtual_addr: address as u64,
            physical_addr: 0,
            file_size: 0,
            mem_size: size as u64,
            align: align as u64,
        };
        // SAFETY: ProgramHeader64 is a repr(C) ELF wire POD; its first field
        // is the four-byte p_type integer. All bit patterns are valid.
        unsafe {
            core::ptr::addr_of_mut!(header).cast::<u32>().write(1);
        }
        header
    }
    #[test]
    fn envelope_accounts_bss_gaps_and_nonzero_origin_without_bias_misalignment() {
        let envelope = ImageEnvelope::from_headers(&[
            load(0x201000, 1, 0x200000),
            load(0x500000, 0x400001, 4096),
        ])
        .unwrap();
        assert_eq!(
            envelope,
            ImageEnvelope {
                start: 0x200000,
                size: 0x800000,
                align: 0x200000
            }
        );
        let limit =
            VirtAddrRange::new(VirtAddr::from_usize(4096), VirtAddr::from_usize(0x40000000));
        let bias = envelope
            .place(0x4000000, limit, |hint, size, actual_limit, align| {
                assert_eq!(hint.as_usize(), 0x4200000);
                assert_eq!(size, 0x800000);
                assert_eq!(actual_limit, limit);
                assert_eq!(align, 0x200000);
                Some(VirtAddr::from_usize(0x8200000))
            })
            .unwrap();
        assert_eq!(bias, 0x8000000);
        assert!(
            envelope
                .place(0x4000000, limit, |_, _, _, _| Some(limit.end))
                .is_err()
        );
        assert!(envelope.place(0x4000000, limit, |_, _, _, _| None).is_err());
    }
    #[test]
    fn invalid_or_wrapping_envelopes_fail_before_placement() {
        assert!(ImageEnvelope::from_headers(&[]).is_err());
        assert!(ImageEnvelope::from_headers(&[load(usize::MAX - 1, 8192, 4096)]).is_err());
    }
}
