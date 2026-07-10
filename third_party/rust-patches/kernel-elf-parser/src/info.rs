//! ELF information parsed from the ELF file

use alloc::vec::Vec;
use core::ops::Range;

use xmas_elf::{
    header::Class,
    program::{ProgramHeader32, ProgramHeader64},
};

use crate::auxv::{AuxEntry, AuxType};

pub struct ELFHeadersBuilder<'a>(ELFHeaders<'a>);
impl<'a> ELFHeadersBuilder<'a> {
    pub fn new(input: &'a [u8]) -> Result<Self, &'static str> {
        Ok(Self(ELFHeaders {
            header: xmas_elf::header::parse_header(input)?,
            ph: Vec::new(),
        }))
    }

    pub fn ph_range(&self) -> Option<Range<u64>> {
        let start = self.0.header.pt2.ph_offset();
        let entry_size = self.expected_ph_entry_size()?;
        if self.0.header.pt2.ph_entry_size() as usize != entry_size {
            return None;
        }
        let size = (entry_size as u64)
            .checked_mul(self.0.header.pt2.ph_count() as u64)?;
        Some(start..start.checked_add(size)?)
    }

    fn expected_ph_entry_size(&self) -> Option<usize> {
        match self.0.header.pt1.class() {
            Class::ThirtyTwo => Some(core::mem::size_of::<ProgramHeader32>()),
            Class::SixtyFour => Some(core::mem::size_of::<ProgramHeader64>()),
            Class::None | Class::Other(_) => None,
        }
    }

    pub fn build(mut self, ph: &[u8]) -> Result<ELFHeaders<'a>, &'static str> {
        let entry_size = self.0.header.pt2.ph_entry_size() as usize;
        let expected_entry_size = self
            .expected_ph_entry_size()
            .ok_or("unsupported ELF class")?;
        if entry_size != expected_entry_size {
            return Err("invalid program header entry size");
        }
        let expected_len = entry_size
            .checked_mul(self.0.header.pt2.ph_count() as usize)
            .ok_or("program header table is too large")?;
        if ph.len() != expected_len {
            return Err("incomplete program header table");
        }

        self.0.ph = ph
            .chunks_exact(entry_size)
            .map(|chunk| match self.0.header.pt1.class() {
                Class::ThirtyTwo => {
                    // The entry size was validated above; ELF permits byte-aligned tables.
                    let ph = unsafe { chunk.as_ptr().cast::<ProgramHeader32>().read_unaligned() };
                    ProgramHeader64 {
                        type_: ph.type_,
                        offset: ph.offset as _,
                        virtual_addr: ph.virtual_addr as _,
                        physical_addr: ph.physical_addr as _,
                        file_size: ph.file_size as _,
                        mem_size: ph.mem_size as _,
                        flags: ph.flags,
                        align: ph.align as _,
                    }
                }
                // ProgramHeader64 contains only integer fields and accepts every bit pattern.
                Class::SixtyFour => unsafe {
                    chunk.as_ptr().cast::<ProgramHeader64>().read_unaligned()
                },
                Class::None | Class::Other(_) => unreachable!(),
            })
            .collect();
        Ok(self.0)
    }
}

pub struct ELFHeaders<'a> {
    pub header: xmas_elf::header::Header<'a>,
    pub ph: Vec<ProgramHeader64>,
}

/// A wrapper for the ELF file data with some useful methods.
pub struct ELFParser<'a> {
    headers: &'a ELFHeaders<'a>,
    /// Base address of the ELF file loaded into the memory.
    base: usize,
    entry: usize,
    phdr: usize,
}

impl<'a> ELFParser<'a> {
    /// Create a new `ELFInfo` instance.
    pub fn new(headers: &'a ELFHeaders<'a>, bias: usize) -> Result<Self, &'static str> {
        let base = if headers.header.pt2.type_().as_type() == xmas_elf::header::Type::SharedObject {
            bias
        } else {
            0
        };
        let entry = usize::try_from(headers.header.pt2.entry_point())
            .ok()
            .and_then(|entry| entry.checked_add(base))
            .ok_or("ELF entry address overflow")?;
        let ph_offset = headers.header.pt2.ph_offset();
        let phdr = headers
            .ph
            .iter()
            .find_map(|header| {
                let file_end = header.offset.checked_add(header.file_size)?;
                (header.offset..file_end)
                    .contains(&ph_offset)
                    .then(|| {
                        ph_offset
                            .checked_sub(header.offset)?
                            .checked_add(header.virtual_addr)?
                            .checked_add(u64::try_from(base).ok()?)
                    })?
            })
            .and_then(|address| usize::try_from(address).ok())
            .ok_or("program header table is not mapped")?;
        Ok(Self {
            headers,
            base,
            entry,
            phdr,
        })
    }

    /// The entry point of the ELF file.
    pub fn entry(&self) -> usize {
        self.entry
    }

    /// The number of program headers in the ELF file.
    pub fn phnum(&self) -> usize {
        self.headers.header.pt2.ph_count() as usize
    }

    /// The size of the program header table entry in the ELF file.
    pub fn phent(&self) -> usize {
        self.headers.header.pt2.ph_entry_size() as usize
    }

    /// The offset of the program header table in the ELF file.
    pub fn phdr(&self) -> usize {
        self.phdr
    }

    /// The base address of the ELF file loaded into the memory.
    pub fn base(&self) -> usize {
        self.base
    }

    pub fn headers(&self) -> &'a ELFHeaders<'a> {
        self.headers
    }

    /// Part of auxiliary vectors from the ELF file.
    ///
    /// # Arguments
    ///
    /// * `pagesz` - The page size of the system
    /// * `ldso_base` - The base address of the dynamic linker (if exists)
    ///
    /// Details about auxiliary vectors are described in <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>
    pub fn aux_vector(
        &self,
        pagesz: usize,
        ldso_base: Option<usize>,
    ) -> impl Iterator<Item = AuxEntry> {
        [
            (AuxType::PHDR, self.phdr()),
            (AuxType::PHENT, self.phent()),
            (AuxType::PHNUM, self.phnum()),
            (AuxType::PAGESZ, pagesz),
            (AuxType::ENTRY, self.entry()),
        ]
        .into_iter()
        .chain(ldso_base.into_iter().map(|base| (AuxType::BASE, base)))
        .map(|(at, val)| AuxEntry::new(at, val))
    }
}
