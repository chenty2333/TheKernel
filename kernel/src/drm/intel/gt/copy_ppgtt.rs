// SPDX-License-Identifier: MIT
// Linux7.2.3 gt/gen8_ppgtt.c __gen8_ppgtt_alloc/gen8_ppgtt_insert_pte:
// Copyright ©2020 Intel Corporation. MIT grant: tk-intel-gt/LICENSE-MIT.
//! Four-level 4K system-memory path. The VM gate and stopped engine replace
//! upstream directory locks; allocate/fill the entire stash before publishing
//! any parent. DMA pins and charges follow the retained native job, including
//! quarantine. Huge/local-memory mappings are not admitted.
use alloc::{sync::Arc, vec::Vec};

use super::{Error, PAGE, Ram, ppgtt};
use crate::drm::gem::GemMemoryCharge;

pub(super) struct Mapping<'a> {
    pub address: u64,
    pub pages: &'a [u64],
    pub writable: bool,
    pub pat: u8,
}
pub(super) struct Stash {
    pub tables: Vec<Ram>,
    pub root: Vec<u64>,
}
#[cfg(test)]
impl Stash {
    pub fn publish(&self, root: &Ram) -> Result<(), Error> {
        root.table(0, self.root.as_slice().try_into().unwrap())?;
        root.flush();
        Ok(())
    }
}
struct Node {
    level: u8,
    prefix: u64,
    ram: Ram,
    words: Vec<u64>,
}
pub(super) fn normalize(value: u64) -> Result<u64, Error> {
    let low = value & ((1u64 << 48) - 1);
    let canonical = if low & (1 << 47) != 0 {
        low | (!0u64 << 48)
    } else {
        low
    };
    if value != low && value != canonical {
        return Err(Error::Refused);
    }
    Ok(low)
}
pub(super) fn checked_range(value: u64, count: usize) -> Result<(u64, u64), Error> {
    let start = normalize(value)?;
    let length = (count as u64)
        .checked_mul(PAGE as u64)
        .ok_or(Error::Refused)?;
    let end = start.checked_add(length).ok_or(Error::Refused)?;
    if start == 0 || !start.is_multiple_of(PAGE as u64) || length == 0 || end > 1 << 48 {
        return Err(Error::Refused);
    }
    Ok((start, end))
}
fn range(m: &Mapping<'_>) -> Result<(u64, u64), Error> {
    let start = normalize(m.address)?;
    let length = (m.pages.len() as u64)
        .checked_mul(PAGE as u64)
        .ok_or(Error::Refused)?;
    let end = start.checked_add(length).ok_or(Error::Refused)?;
    if start == 0 || !start.is_multiple_of(PAGE as u64) || length == 0 || end > 1 << 48 {
        return Err(Error::Refused);
    }
    for &physical in m.pages {
        ppgtt::pte(physical, m.pat, m.writable)?;
    }
    Ok((start, end))
}
/// Publish only with no outstanding users of this root. Retain returned tables
/// until scoped reset retirement, NOT merely a breadcrumb or timeout.
pub(super) fn populate(
    root: &Ram,
    mappings: &[Mapping<'_>],
    charge: Option<&Arc<GemMemoryCharge>>,
) -> Result<Stash, Error> {
    for (i, m) in mappings.iter().enumerate() {
        let (start, end) = range(m)?;
        for previous in &mappings[..i] {
            let (a, b) = range(previous)?;
            if start < b && a < end {
                return Err(Error::Refused);
            }
        }
    }
    let mut nodes: Vec<Node> = Vec::new();
    let mut root_words = super::zero_words::<u64>(512)?;
    root_words.fill(ppgtt::pde(root.physical[4])?);
    for mapping in mappings {
        let start = normalize(mapping.address)?;
        for (index, &physical) in mapping.pages.iter().enumerate() {
            let va = start + index as u64 * PAGE as u64;
            let mut parent: Option<usize> = None;
            for level in (0..=2u8).rev() {
                let prefix = va >> (21 + u32::from(level) * 9);
                let node = match nodes
                    .iter()
                    .position(|n| n.level == level && n.prefix == prefix)
                {
                    Some(index) => index,
                    None => {
                        // Bound stash bookkeeping independently of available RAM.
                        if nodes.len() == 4096 {
                            return Err(Error::Refused);
                        }
                        nodes.try_reserve(1).map_err(|_| Error::Refused)?;
                        let ram = Ram::allocate(1)?;
                        if let Some(charge) = charge {
                            let child = charge.reserve_related(PAGE).map_err(|_| Error::Refused)?;
                            ram.pages
                                .retain_allocation_owner(child)
                                .map_err(|_| Error::Refused)?;
                        }
                        let mut words = super::zero_words::<u64>(512)?;
                        let scratch = if level == 0 {
                            ppgtt::pte(root.physical[7], 3, false)?
                        } else {
                            ppgtt::pde(root.physical[6 - usize::from(level) + 1])?
                        };
                        words.fill(scratch);
                        nodes.push(Node {
                            level,
                            prefix,
                            ram,
                            words,
                        });
                        nodes.len() - 1
                    }
                };
                let entry = ppgtt::pde(nodes[node].ram.physical[0])?;
                let slot = ((va >> (21 + u32::from(level) * 9)) & 511) as usize;
                if let Some(parent) = parent {
                    nodes[parent].words[slot] = entry;
                } else {
                    root_words[slot] = entry;
                }
                parent = Some(node);
            }
            nodes[parent.unwrap()].words[((va >> 12) & 511) as usize] =
                ppgtt::pte(physical, mapping.pat, mapping.writable)?;
        }
    }
    // Leaf -> directory cache publication, root last, as upstream flushes each
    // completed leaf before moving on. No allocation remains after publication.
    let mut retained = Vec::new();
    retained
        .try_reserve_exact(nodes.len())
        .map_err(|_| Error::Refused)?;
    for level in 0..=2 {
        for node in nodes.iter().filter(|n| n.level == level) {
            node.ram
                .table(0, node.words.as_slice().try_into().unwrap())?;
            node.ram.flush();
        }
    }
    for node in nodes {
        retained.push(node.ram);
    }
    Ok(Stash {
        tables: retained,
        root: root_words,
    })
}
