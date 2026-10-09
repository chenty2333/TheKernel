//! Interrupt-remapping table and MSI/IOAPIC entry adaptation.
//!
//! Translated from FreeBSD `sys/x86/iommu/intel_intrmap.c` (BSD-2-Clause;
//! FreeBSD source snapshot 2026-10-08). Copyright (c) 2015 The FreeBSD
//! Foundation. This software was developed by Konstantin Belousov
//! <kib@FreeBSD.org> under sponsorship from the FreeBSD Foundation.
//! `device_t`, VMEM, interrupt-controller reprogramming and QI/MMIO are passed
//! through native resolver/IO adapters rather than importing FreeBSD frameworks.

use alloc::{vec, vec::Vec};

use crate::{
    Error,
    reg::{
        DMAR_IRTE1_DLM_ExtINT, DMAR_IRTE1_DLM_FM, DMAR_IRTE1_DLM_NMI, DMAR_IRTE1_DLM_SMI,
        DMAR_IRTE1_DM_PHYSICAL, DMAR_IRTE1_DST_x2APIC, DMAR_IRTE1_DST_xAPIC, DMAR_IRTE1_P,
        DMAR_IRTE1_RH_DIRECT, DMAR_IRTE1_TM_EDGE, DMAR_IRTE1_TM_LEVEL, DMAR_IRTE1_V,
        DMAR_IRTE2_SID_RID, DMAR_IRTE2_SQ_RID, DMAR_IRTE2_SVT_RID, Irte,
    },
};

const MSI_INTEL_ADDR_BASE: u64 = 0xfee0_0000;
const IOART_TRGREDG: u64 = 0;
const IOART_TRGRLVL: u64 = 1 << 15;
const IOART_INTAHI: u64 = 0;
const IOART_INTALO: u64 = 1 << 13;
const IOART_DELFIXED: u64 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptSource {
    Dmar,
    Hpet {
        unit: Option<usize>,
        requester_id: u16,
    },
    Pci {
        unit: Option<usize>,
        requester_id: u16,
    },
}

impl InterruptSource {
    // upstream: intel_intrmap.c dmar_ir_find()
    fn resolve(self) -> (Option<usize>, Option<u16>, bool) {
        match self {
            Self::Dmar => (None, None, true),
            Self::Hpet { unit, requester_id } | Self::Pci { unit, requester_id } => {
                (unit, Some(requester_id), false)
            }
        }
    }
}

/// Native hardware hooks for the IEC invalidation and IRTA/IRE programming.
pub trait InterruptRemapIo {
    /// Store the entry and make it visible to the remapper. Return
    /// [`Error::Quarantined`] if the store may have reached hardware but its
    /// visibility cannot be confirmed; other errors must mean no publication
    /// occurred.
    fn store_irte(&mut self, index: u16, entry: Irte) -> Result<(), Error>;
    fn invalidate_iec(&mut self, index: u16, count: u16) -> Result<(), Error>;
    fn invalidate_iec_global(&mut self) -> Result<(), Error>;
    fn load_table_pointer(&mut self, physical: u64, size_order: u8) -> Result<(), Error>;
    fn enable_interrupt_remapping(&mut self) -> Result<(), Error>;
    fn disable_interrupt_remapping(&mut self) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryMode {
    Fixed,
    ExtInt,
    Nmi,
    Smi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicRoute {
    pub cookie: u16,
    pub high: u32,
    pub low: u32,
}

pub struct InterruptRemapper {
    entries: Vec<Irte>,
    allocated: Vec<bool>,
    enabled: bool,
    x2apic: bool,
}

impl InterruptRemapper {
    pub fn new(entry_count: usize, x2apic: bool) -> Result<Self, Error> {
        if entry_count == 0 || entry_count > u16::MAX as usize + 1 || !entry_count.is_power_of_two()
        {
            return Err(Error::InvalidRange);
        }
        Ok(Self {
            entries: vec![Irte::default(); entry_count],
            allocated: vec![false; entry_count],
            enabled: false,
            x2apic,
        })
    }

    pub const fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Allocate a contiguous first-fit range of IRTE cookies.
    // upstream: intel_intrmap.c dmar_alloc_msi_intr()
    pub fn allocate_msi<I: InterruptRemapIo>(
        &mut self,
        _io: &mut I,
        count: usize,
    ) -> Result<Vec<u16>, Error> {
        if !self.enabled {
            return Err(Error::Unsupported);
        }
        if count == 0 || count > self.allocated.len() {
            return Err(Error::InvalidRange);
        }
        let start = self
            .allocated
            .windows(count)
            .position(|window| window.iter().all(|used| !used))
            .ok_or(Error::OutOfMemory)?;
        for slot in &mut self.allocated[start..start + count] {
            *slot = true;
        }
        Ok((start..start + count).map(|index| index as u16).collect())
    }

    /// Program an MSI route or return the direct DMAR non-remapped encoding.
    // upstream: intel_intrmap.c dmar_map_msi_intr()
    pub fn map_msi<I: InterruptRemapIo>(
        &mut self,
        io: &mut I,
        source: InterruptSource,
        cpu: u32,
        vector: u8,
        cookie: u16,
    ) -> Result<(u64, u32), Error> {
        let (unit, requester_id, is_dmar) = source.resolve();
        if is_dmar {
            let mut address = MSI_INTEL_ADDR_BASE | (u64::from(cpu & 0xff) << 12);
            if self.x2apic {
                address |= u64::from(cpu & 0xffff_ff00) << 32;
            } else if cpu > 0xff {
                return Err(Error::InvalidRange);
            }
            return Ok((address, u32::from(vector)));
        }
        if unit.is_none() || !self.enabled || !self.cookie_allocated(cookie) {
            return Err(Error::Unsupported);
        }
        let rid = requester_id.ok_or(Error::NoDevice)?;
        let low = self.destination(cpu)
            | DMAR_IRTE1_V(u64::from(vector))
            | DMAR_IRTE1_DLM_FM
            | DMAR_IRTE1_TM_EDGE
            | DMAR_IRTE1_RH_DIRECT
            | DMAR_IRTE1_DM_PHYSICAL
            | DMAR_IRTE1_P;
        self.program(io, cookie, low, rid)?;
        let address = MSI_INTEL_ADDR_BASE
            | (u64::from(cookie & 0x7fff) << 5)
            | (u64::from(cookie & 0x8000) << 2)
            | 0x18;
        Ok((address, 0))
    }

    /// Clear an MSI entry and return its cookie to the first-fit allocator.
    // upstream: intel_intrmap.c dmar_unmap_msi_intr()
    pub fn unmap_msi<I: InterruptRemapIo>(
        &mut self,
        io: &mut I,
        cookie: Option<u16>,
    ) -> Result<(), Error> {
        if let Some(cookie) = cookie {
            self.free_entry(io, cookie)?;
        }
        Ok(())
    }

    /// Install an IOAPIC IRTE and build the IOAPIC remappable redirection word.
    // upstream: intel_intrmap.c dmar_map_ioapic_intr()
    pub fn map_ioapic<I: InterruptRemapIo>(
        &mut self,
        io: &mut I,
        requester_id: u16,
        cpu: u32,
        vector: u8,
        edge: bool,
        active_high: bool,
        mode: DeliveryMode,
    ) -> Result<IoApicRoute, Error> {
        if !self.enabled {
            return Err(Error::Unsupported);
        }
        let index = self
            .allocated
            .iter()
            .position(|used| !used)
            .ok_or(Error::OutOfMemory)?;
        self.allocated[index] = true;
        let delivery = match mode {
            DeliveryMode::Fixed => {
                if vector == 0 {
                    self.allocated[index] = false;
                    return Err(Error::InvalidRange);
                }
                DMAR_IRTE1_DLM_FM | DMAR_IRTE1_V(u64::from(vector))
            }
            DeliveryMode::ExtInt => DMAR_IRTE1_DLM_ExtINT,
            DeliveryMode::Nmi => DMAR_IRTE1_DLM_NMI,
            DeliveryMode::Smi => DMAR_IRTE1_DLM_SMI,
        };
        let low = self.destination(cpu)
            | delivery
            | if edge {
                DMAR_IRTE1_TM_EDGE
            } else {
                DMAR_IRTE1_TM_LEVEL
            }
            | DMAR_IRTE1_RH_DIRECT
            | DMAR_IRTE1_DM_PHYSICAL
            | DMAR_IRTE1_P;
        if let Err(error) = self.program(io, index as u16, low, requester_id) {
            // A store/invalidation failure after publication leaves the slot
            // potentially visible to hardware. Only return a slot to the
            // allocator when the adapter proves that nothing was published.
            if error != Error::Quarantined {
                self.allocated[index] = false;
            }
            return Err(error);
        }
        let cookie = index as u16;
        let redirection = (1u64 << 48)
            | (u64::from(cookie & 0x7fff) << 49)
            | (if cookie & 0x8000 != 0 { 1u64 << 11 } else { 0 })
            | (if edge { IOART_TRGREDG } else { IOART_TRGRLVL })
            | (if active_high {
                IOART_INTAHI
            } else {
                IOART_INTALO
            })
            | IOART_DELFIXED
            | u64::from(vector);
        Ok(IoApicRoute {
            cookie,
            high: (redirection >> 32) as u32,
            low: redirection as u32,
        })
    }

    /// Invalidate and release an IOAPIC IRTE; an absent cookie is a no-op.
    // upstream: intel_intrmap.c dmar_unmap_ioapic_intr()
    pub fn unmap_ioapic<I: InterruptRemapIo>(
        &mut self,
        io: &mut I,
        cookie: &mut Option<u16>,
    ) -> Result<(), Error> {
        if let Some(index) = *cookie {
            self.free_entry(io, index)?;
            *cookie = None;
        }
        Ok(())
    }

    /// Set/update IRTE words before issuing IEC invalidation.
    // upstream: intel_intrmap.c dmar_ir_program_irte()
    fn program<I: InterruptRemapIo>(
        &mut self,
        io: &mut I,
        index: u16,
        low: u64,
        rid: u16,
    ) -> Result<(), Error> {
        let entry = self
            .entries
            .get_mut(index as usize)
            .ok_or(Error::InvalidRange)?;
        let old_entry = *entry;
        let high = DMAR_IRTE2_SVT_RID | DMAR_IRTE2_SQ_RID | DMAR_IRTE2_SID_RID(u64::from(rid));
        if entry.irte1 & DMAR_IRTE1_P != 0 {
            if entry.irte2 != high {
                return Err(Error::InvalidStructure);
            }
            entry.irte1 = low;
        } else {
            entry.irte2 = high;
            entry.irte1 = low;
        }
        if let Err(error) = io.store_irte(index, *entry) {
            if error != Error::Quarantined {
                *entry = old_entry;
            }
            return Err(error);
        }
        // The IRTE has been published. Any IEC failure makes its cache state
        // ambiguous, regardless of the lower-level timeout/error code.
        io.invalidate_iec(index, 1).map_err(|_| Error::Quarantined)
    }

    /// Clear both IRTE words, invalidate, and release the allocated cookie.
    // upstream: intel_intrmap.c dmar_ir_free_irte()
    fn free_entry<I: InterruptRemapIo>(&mut self, io: &mut I, index: u16) -> Result<(), Error> {
        let entry = self
            .entries
            .get_mut(index as usize)
            .ok_or(Error::InvalidRange)?;
        let old_entry = *entry;
        entry.irte1 = 0;
        entry.irte2 = 0;
        if let Err(error) = io.store_irte(index, *entry) {
            if error != Error::Quarantined {
                *entry = old_entry;
            }
            return Err(error);
        }
        // Do not free the slot until the clear is globally visible to lookup.
        io.invalidate_iec(index, 1)
            .map_err(|_| Error::Quarantined)?;
        self.allocated[index as usize] = false;
        Ok(())
    }

    /// Initialize IRTA and enable IR only when capability and QI are available.
    // upstream: intel_intrmap.c dmar_init_irt()
    pub fn initialize<I: InterruptRemapIo>(
        &mut self,
        io: &mut I,
        capable: bool,
        qi_enabled: bool,
        physical: u64,
    ) -> Result<bool, Error> {
        if !capable {
            return Ok(false);
        }
        if !qi_enabled {
            return Ok(false);
        }
        let order = self.entries.len().trailing_zeros() as u8;
        io.load_table_pointer(physical, order)?;
        io.invalidate_iec_global()?;
        io.enable_interrupt_remapping()?;
        self.enabled = true;
        Ok(true)
    }

    /// Disable IR before global IEC invalidation and clear local state.
    // upstream: intel_intrmap.c dmar_fini_irt()
    pub fn finalize<I: InterruptRemapIo>(&mut self, io: &mut I) -> Result<(), Error> {
        self.enabled = false;
        io.disable_interrupt_remapping()?;
        io.invalidate_iec_global()?;
        self.entries.fill(Irte::default());
        self.allocated.fill(false);
        Ok(())
    }

    fn cookie_allocated(&self, cookie: u16) -> bool {
        self.allocated
            .get(cookie as usize)
            .copied()
            .unwrap_or(false)
    }
    fn destination(&self, cpu: u32) -> u64 {
        if self.x2apic {
            DMAR_IRTE1_DST_x2APIC(u64::from(cpu))
        } else {
            DMAR_IRTE1_DST_xAPIC(u64::from(cpu))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        invalidated: Vec<(u16, u16)>,
        global: usize,
        enabled: bool,
        stored: Vec<(u16, Irte)>,
        store_error: Option<Error>,
        store_error_after_publish: bool,
        invalidate_error: Option<Error>,
    }
    impl InterruptRemapIo for Fake {
        fn store_irte(&mut self, index: u16, entry: Irte) -> Result<(), Error> {
            if let Some(error) = self.store_error.take() {
                if self.store_error_after_publish {
                    self.stored.push((index, entry));
                }
                return Err(error);
            }
            self.stored.push((index, entry));
            Ok(())
        }
        fn invalidate_iec(&mut self, index: u16, count: u16) -> Result<(), Error> {
            self.invalidated.push((index, count));
            self.invalidate_error.take().map_or(Ok(()), Err)
        }
        fn invalidate_iec_global(&mut self) -> Result<(), Error> {
            self.global += 1;
            Ok(())
        }
        fn load_table_pointer(&mut self, _: u64, _: u8) -> Result<(), Error> {
            Ok(())
        }
        fn enable_interrupt_remapping(&mut self) -> Result<(), Error> {
            self.enabled = true;
            Ok(())
        }
        fn disable_interrupt_remapping(&mut self) -> Result<(), Error> {
            self.enabled = false;
            Ok(())
        }
    }

    #[test]
    fn initialization_requires_qi_and_installs_table_before_enable() {
        let mut ir = InterruptRemapper::new(16, false).unwrap();
        let mut io = Fake::default();
        assert!(!ir.initialize(&mut io, true, false, 0x1000).unwrap());
        assert!(ir.initialize(&mut io, true, true, 0x1000).unwrap());
        assert!(io.enabled);
        assert_eq!(io.global, 1);
    }

    #[test]
    fn msi_and_ioapic_routes_program_and_release_irte() {
        let mut ir = InterruptRemapper::new(16, true).unwrap();
        let mut io = Fake::default();
        ir.initialize(&mut io, true, true, 0x2000).unwrap();
        let source = InterruptSource::Pci {
            unit: Some(0),
            requester_id: 0x1234,
        };
        let cookie = ir.allocate_msi(&mut io, 1).unwrap()[0];
        let (address, data) = ir.map_msi(&mut io, source, 0x123, 0x51, cookie).unwrap();
        assert_eq!(data, 0);
        assert_eq!(
            address,
            MSI_INTEL_ADDR_BASE | (u64::from(cookie) << 5) | 0x18
        );
        assert_eq!(ir.entries[cookie as usize].irte2 & 0xffff, 0x1234);
        ir.unmap_msi(&mut io, Some(cookie)).unwrap();
        assert_eq!(ir.entries[cookie as usize], Irte::default());
        let route = ir
            .map_ioapic(&mut io, 0x4321, 3, 0x41, false, true, DeliveryMode::Fixed)
            .unwrap();
        assert_eq!(route.cookie, 0);
        let mut cookie = Some(route.cookie);
        ir.unmap_ioapic(&mut io, &mut cookie).unwrap();
        assert_eq!(cookie, None);
    }

    #[test]
    fn ioapic_map_quarantines_slot_after_ambiguous_store_or_iec_failure() {
        for fail_store_after_publish in [true, false] {
            let mut ir = InterruptRemapper::new(2, true).unwrap();
            let mut io = Fake::default();
            ir.initialize(&mut io, true, true, 0x2000).unwrap();
            if fail_store_after_publish {
                io.store_error = Some(Error::Quarantined);
                io.store_error_after_publish = true;
            } else {
                io.invalidate_error = Some(Error::Timeout);
            }

            assert_eq!(
                ir.map_ioapic(&mut io, 0x4321, 3, 0x41, false, true, DeliveryMode::Fixed),
                Err(Error::Quarantined)
            );
            assert!(ir.allocated[0], "ambiguous slot must remain reserved");
            assert_ne!(ir.entries[0].irte1 & DMAR_IRTE1_P, 0);

            io.store_error = None;
            io.invalidate_error = None;
            let next = ir
                .map_ioapic(&mut io, 0x4321, 3, 0x42, false, true, DeliveryMode::Fixed)
                .unwrap();
            assert_eq!(next.cookie, 1, "the possibly published slot is not reused");
        }
    }

    #[test]
    fn ioapic_unmap_retains_cookie_and_slot_until_iec_succeeds() {
        let mut ir = InterruptRemapper::new(2, true).unwrap();
        let mut io = Fake::default();
        ir.initialize(&mut io, true, true, 0x2000).unwrap();
        let route = ir
            .map_ioapic(&mut io, 0x4321, 3, 0x41, false, true, DeliveryMode::Fixed)
            .unwrap();
        let mut cookie = Some(route.cookie);

        io.invalidate_error = Some(Error::Timeout);
        assert_eq!(
            ir.unmap_ioapic(&mut io, &mut cookie),
            Err(Error::Quarantined)
        );
        assert_eq!(cookie, Some(route.cookie));
        assert!(ir.allocated[route.cookie as usize]);

        assert_eq!(ir.unmap_ioapic(&mut io, &mut cookie), Ok(()));
        assert_eq!(cookie, None);
        assert!(!ir.allocated[route.cookie as usize]);
    }

    #[test]
    fn ioapic_map_releases_slot_when_store_proves_no_publication() {
        let mut ir = InterruptRemapper::new(1, true).unwrap();
        let mut io = Fake::default();
        ir.initialize(&mut io, true, true, 0x2000).unwrap();
        io.store_error = Some(Error::InvalidRange);

        assert_eq!(
            ir.map_ioapic(&mut io, 0x4321, 3, 0x41, false, true, DeliveryMode::Fixed),
            Err(Error::InvalidRange)
        );
        assert!(!ir.allocated[0]);
        assert_eq!(ir.entries[0], Irte::default());

        io.store_error = None;
        let route = ir
            .map_ioapic(&mut io, 0x4321, 3, 0x42, false, true, DeliveryMode::Fixed)
            .unwrap();
        assert_eq!(route.cookie, 0);
    }

    #[test]
    fn dmar_msi_stays_direct_and_allocators_fail_closed() {
        let mut ir = InterruptRemapper::new(8, false).unwrap();
        let mut io = Fake::default();
        assert_eq!(
            ir.map_msi(&mut io, InterruptSource::Dmar, 2, 0x40, u16::MAX)
                .unwrap(),
            (MSI_INTEL_ADDR_BASE | (2 << 12), 0x40)
        );
        assert_eq!(ir.allocate_msi(&mut io, 1), Err(Error::Unsupported));
        assert_eq!(
            ir.map_msi(
                &mut io,
                InterruptSource::Pci {
                    unit: Some(0),
                    requester_id: 1
                },
                0,
                1,
                0
            ),
            Err(Error::Unsupported)
        );
    }
}
