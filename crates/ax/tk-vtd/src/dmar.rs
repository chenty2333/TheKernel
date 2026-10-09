//! Rust storage model for FreeBSD `sys/x86/iommu/intel_dmar.h`.
//! FreeBSD source snapshot c2b7fe4 (BSD-2-Clause), Copyright (c) 2013-2015
//! The FreeBSD Foundation; Konstantin Belousov, under Foundation sponsorship.
//! TheKernel's PCI/VM/task resources and locks are owned by its DMAR manager,
//! not represented as raw FreeBSD framework pointers. See LICENSES/BSD-2-Clause.txt.

use alloc::vec::Vec;

use crate::reg::{Irte, RootEntry};

pub const DMAR_INTR_FAULT: usize = 0;
pub const DMAR_INTR_QI: usize = 1;
pub const DMAR_INTR_TOTAL: usize = 2;
pub const DMAR_BARRIER_RMRR: usize = 0;
pub const DMAR_BARRIER_USEQ: usize = 1;

/// `DMAR_IS_COHERENT`: hardware advertises coherent translation structures.
pub const fn dmar_is_coherent(hw_ecap: u64) -> bool {
    hw_ecap & crate::reg::DMAR_ECAP_C != 0
}
/// `DMAR_HAS_QI`: hardware implements queued invalidation descriptors.
pub const fn dmar_has_qi(hw_ecap: u64) -> bool {
    hw_ecap & crate::reg::DMAR_ECAP_QI != 0
}
/// `DMAR_X2APIC`: x2APIC interrupt format is usable only when mode and EIM agree.
pub const fn dmar_x2apic(x2apic_mode: bool, hw_ecap: u64) -> bool {
    x2apic_mode && hw_ecap & crate::reg::DMAR_ECAP_EIM != 0
}

/// FreeBSD dmar_domain fields after mapping iommu_domain/VM object/locks into
/// the manager-owned `SecondLevel` page-table and IOVA components.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DmarDomain {
    /// Domain identifier written into context entries (upstream `domain`).
    pub domain_id: i32,
    pub mgaw: i32,
    pub agaw: i32,
    pub page_level: i32,
    /// `awlvl` encoding stored in the second context-entry word.
    pub awlvl: i32,
    pub context_count: u32,
    pub refs: u32,
    pub unit_index: u32,
    /// Physical root of the page-table object owned by the platform allocator.
    pub page_table_root: u64,
    pub batch_no: u32,
}

/// FreeBSD dmar_ctx: generic PCI requester binding is represented by the
/// source-id/domain pair; fault record history remains per context.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DmarContext {
    pub source_id: u16,
    pub domain_id: i32,
    pub last_fault_rec: [u64; 2],
}

/// Fault taskqueue/lock and generic resource handles are mapped to kernel
/// interrupt/task and MMIO ownership. Their durable hardware state is retained
/// here so the implementation can be ported field-for-field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FaultLogState {
    pub records: Vec<[u64; 2]>,
    pub head: usize,
    pub tail: usize,
}

/// Hardware-owned fields of FreeBSD `struct dmar_unit`. The surrounding IOMMU
/// framework object, MMIO resource handle, domain-ID allocator, fault lock,
/// and taskqueue are TheKernel-owned interfaces rather than copied frameworks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DmarUnit {
    pub segment: u16,
    pub register_base: u64,
    pub memory_domain: i32,
    pub register_resource_id: i32,
    pub hw_ver: u32,
    pub hw_cap: u64,
    pub hw_ecap: u64,
    pub hw_gcmd: u32,
    pub domains: Vec<DmarDomain>,
    pub domain_ids: Vec<u32>,
    pub root_table: Vec<RootEntry>,
    pub context_table_roots: Vec<u64>,
    pub barrier_flags: u32,
    pub fault_log: FaultLogState,
    pub qi_enabled: bool,
    pub ir_enabled: bool,
    pub irt_phys: u64,
    pub irt: Vec<Irte>,
    pub irte_count: u32,
    pub irte_ids: Vec<u32>,
}

impl DmarUnit {
    pub const fn is_coherent(&self) -> bool {
        dmar_is_coherent(self.hw_ecap)
    }

    pub const fn has_queued_invalidation(&self) -> bool {
        dmar_has_qi(self.hw_ecap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translated_unit_flags_match_the_header_helpers() {
        assert_eq!(DMAR_INTR_FAULT, 0);
        assert_eq!(DMAR_INTR_QI, 1);
        assert_eq!(DMAR_INTR_TOTAL, 2);
        assert_eq!(DMAR_BARRIER_RMRR, 0);
        assert_eq!(DMAR_BARRIER_USEQ, 1);
        assert!(dmar_is_coherent(crate::reg::DMAR_ECAP_C));
        assert!(dmar_has_qi(crate::reg::DMAR_ECAP_QI));
        assert!(dmar_x2apic(true, crate::reg::DMAR_ECAP_EIM));
        assert!(!dmar_x2apic(false, crate::reg::DMAR_ECAP_EIM));
    }

    #[test]
    fn model_retains_hardware_domain_context_and_fault_state() {
        let domain = DmarDomain {
            domain_id: 7,
            mgaw: 48,
            agaw: 48,
            page_level: 4,
            awlvl: crate::reg::DMAR_CTX2_AW_4LVL as i32,
            context_count: 1,
            refs: 2,
            unit_index: 0,
            page_table_root: 0x1000,
            batch_no: 3,
        };
        let context = DmarContext {
            source_id: 0x1234,
            domain_id: domain.domain_id,
            last_fault_rec: [0x55, 0xaa],
        };
        assert_eq!(context.source_id, 0x1234);
        assert_eq!(context.last_fault_rec, [0x55, 0xaa]);
        assert_eq!(domain.page_table_root & 0xfff, 0);
    }
}
