//! Already observed native BARs. Neither cache lookup nor publication probes hardware.
use alloc::vec::Vec;

#[cfg(bus = "pci")]
use axsync::spin::SpinNoIrq;

use crate::{pci::Address, prelude::*};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Resource {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
}

impl Resource {
    /// Actual size from the existing boot/configuration owner's BAR probe.
    pub(crate) fn observed(start: u64, size: u64, bar_bits: u32) -> Option<Self> {
        if size == 0 {
            return Some(Self::default());
        }
        let end = start.checked_add(size.checked_sub(1)?)?;
        let flags = if bar_bits & 1 != 0 {
            0x0004_0100 | u64::from(bar_bits & 3)
        } else {
            0x0004_0200
                | u64::from(bar_bits & 15)
                | if bar_bits & 8 != 0 { 0x2000 } else { 0 }
                | if bar_bits & 6 == 4 { 0x0010_0000 } else { 0 }
        };
        Some(Self { start, end, flags })
    }
}

#[derive(Clone, Copy)]
struct Entry {
    address: Address,
    identity: [u32; 2],
    bars: [u32; 6],
    resources: [Resource; 6],
}

impl Entry {
    fn matches(&self, identity: [u32; 2], bars: [u32; 6]) -> bool {
        self.identity == identity && self.bars == bars
    }
}

// Same bounded publication class as the kernel's boot device registry. Cache
// exhaustion loses observations, never changes device admission or I/O.
#[cfg(bus = "pci")]
static ENTRIES: SpinNoIrq<[Option<Entry>; 64]> = SpinNoIrq::new([None; 64]);

#[cfg(bus = "pci")]
fn current_signature(address: Address) -> Option<([u32; 2], [u32; 6])> {
    let header = crate::pci::header(address)?;
    let mut bars = [0; 6];
    for (bar, bytes) in bars.iter_mut().zip(header[0x10..0x28].as_chunks::<4>().0) {
        *bar = u32::from_le_bytes(*bytes);
    }
    Some((
        [
            u32::from_le_bytes(header[..4].try_into().ok()?),
            u32::from_le_bytes(header[8..12].try_into().ok()?),
        ],
        bars,
    ))
}

#[cfg(bus = "pci")]
pub(crate) fn record(address: Address, resources: [Resource; 6]) {
    let Some((identity, bars)) = current_signature(address) else {
        return;
    };
    let mut entries = ENTRIES.lock();
    let index = entries
        .iter()
        .position(|entry| entry.is_some_and(|entry| entry.address == address))
        .or_else(|| entries.iter().position(Option::is_none));
    if let Some(index) = index {
        entries[index] = Some(Entry {
            address,
            identity,
            bars,
            resources,
        });
    } else {
        warn!("PCI BAR observation cache full at {address}; preserving device behavior");
    }
}

#[cfg(bus = "pci")]
pub(crate) fn invalidate(address: Address) {
    for entry in ENTRIES.lock().iter_mut() {
        if entry.is_some_and(|entry| entry.address == address) {
            *entry = None;
        }
    }
}

/// Known six-BAR prefix only. ROM/bridge windows were not sized by this owner;
/// EOF leaves them unknown so consumers can fall back to read-only config.
/// An unobserved or changed function yields no rows, not invented zero BARs.
#[cfg(bus = "pci")]
fn observed_prefix(address: Address) -> DevResult<Vec<Resource>> {
    let signature = current_signature(address);
    let entry = ENTRIES
        .lock()
        .iter()
        .flatten()
        .find(|entry| entry.address == address)
        .copied();
    let mut out = Vec::new();
    if let (Some((identity, bars)), Some(entry)) = (signature, entry)
        && entry.matches(identity, bars)
    {
        out.try_reserve_exact(6).map_err(|_| DevError::NoMemory)?;
        out.extend_from_slice(&entry.resources);
    }
    Ok(out)
}

pub fn prefix(address: Address) -> DevResult<Vec<Resource>> {
    #[cfg(bus = "pci")]
    {
        observed_prefix(address)
    }
    #[cfg(not(bus = "pci"))]
    {
        let _ = address;
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observed_bars_preserve_ranges_flags_and_real_absence() {
        assert_eq!(
            Resource::observed(0x380000000000, 0x4000, 0xc),
            Some(Resource {
                start: 0x380000000000,
                end: 0x380000003fff,
                flags: 0x0014_220c
            })
        );
        assert_eq!(
            Resource::observed(0x6060, 0x20, 1),
            Some(Resource {
                start: 0x6060,
                end: 0x607f,
                flags: 0x0004_0101
            })
        );
        assert_eq!(Resource::observed(0, 0, 0), Some(Resource::default()));
        assert_eq!(Resource::observed(u64::MAX, 2, 0), None);
    }
    #[test]
    fn changed_configuration_or_identity_cannot_reuse_stale_observations() {
        let entry = Entry {
            address: Address {
                segment: 0,
                bus: 0,
                device: 1,
                function: 0,
            },
            identity: [0x1234abcd, 0x03000001],
            bars: [1, 2, 3, 4, 5, 6],
            resources: [Resource::default(); 6],
        };
        assert!(entry.matches(entry.identity, entry.bars));
        assert!(!entry.matches([0x5678abcd, 0x03000001], entry.bars));
        let mut bars = entry.bars;
        bars[4] += 0x4000;
        assert!(!entry.matches(entry.identity, bars));
    }
}
