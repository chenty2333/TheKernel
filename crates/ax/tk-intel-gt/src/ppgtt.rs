// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/gen8_ppgtt.c:
// gen8_pde_encode, gen12_pte_encode (system memory,4K,48-bit VM).
// Copyright © 2020 Intel Corporation. Full MIT grant: ../LICENSE-MIT.
// Original bounded table construction for one private low-2MiB BCS VM. Caller
// owns/pins every page and publishes CPU cache contents before GPU execution.
use crate::Error;
pub const ENTRIES: usize = 512;
pub fn physical(address: u64) -> Result<(), Error> {
    if address == 0 || !address.is_multiple_of(4096) || address >= 1 << 39 {
        Err(Error::Refused)
    } else {
        Ok(())
    }
}
pub fn pde(address: u64) -> Result<u64, Error> {
    physical(address)?;
    Ok(address | 3 | 24)
}
pub fn pte(address: u64, pat: u8, writable: bool) -> Result<u64, Error> {
    physical(address)?;
    if pat > 7 {
        return Err(Error::Refused);
    }
    Ok(address
        | 1
        | if writable { 2 } else { 0 }
        | (u64::from(pat & 1) << 3)
        | (u64::from((pat >> 1) & 1) << 4)
        | (u64::from((pat >> 2) & 1) << 7))
}
/// Every unused branch walks dedicated scratch tables, ultimately a READ-ONLY
/// owned zero page. No absent/unknown branch is fabricated into an identity map.
pub fn directory(out: &mut [u64; ENTRIES], scratch: u64, first: u64) -> Result<(), Error> {
    let scratch = pde(scratch)?;
    let first = pde(first)?;
    out.fill(scratch);
    out[0] = first;
    Ok(())
}
pub fn leaf(out: &mut [u64; ENTRIES], scratch: u64, pat: u8) -> Result<(), Error> {
    out.fill(pte(scratch, pat, false)?);
    Ok(())
}
/// Validate the whole range/PTE set before changing a single table entry.
pub fn map(
    out: &mut [u64; ENTRIES],
    address: u64,
    pages: &[u64],
    pat: u8,
    writable: bool,
) -> Result<(), Error> {
    if !address.is_multiple_of(4096) || pages.is_empty() {
        return Err(Error::Refused);
    }
    let start = usize::try_from(address / 4096).map_err(|_| Error::Refused)?;
    let end = start.checked_add(pages.len()).ok_or(Error::Refused)?;
    if start == 0 || end > ENTRIES {
        return Err(Error::Refused);
    }
    for &p in pages {
        pte(p, pat, writable)?;
    }
    for (entry, &p) in out[start..end].iter_mut().zip(pages) {
        *entry = pte(p, pat, writable)?;
    }
    Ok(())
}
