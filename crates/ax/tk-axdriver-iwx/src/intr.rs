//! ICT and interrupt-cause processing from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230,
//! `iwx_ict_reset()`, `iwx_intr()`, and `iwx_intr_msix()` plus `if_iwxreg.h`
//! (ISC). Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c)
//! 2014 Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{
    CsrAccess, DmaAllocator, DmaError, DmaRegion, InterruptMasks, IwxRegisters, disable_interrupts,
    enable_interrupts,
};

pub const ICT_SIZE_BYTES: usize = 4096;
pub const ICT_ENTRY_COUNT: usize = ICT_SIZE_BYTES / 4;
pub const ICT_ADDRESS_SHIFT: u32 = 12;

const CSR_INT: u32 = 0x008;
const CSR_INT_MASK: u32 = 0x00c;
const CSR_FH_INT_STATUS: u32 = 0x010;
const CSR_DRAM_INT_TBL: u32 = 0x0a0;
const CSR_DRAM_INT_TBL_ENABLE: u32 = 1 << 31;
const CSR_DRAM_INT_TBL_WRITE_POINTER: u32 = 1 << 28;
const CSR_DRAM_INT_TBL_WRAP_CHECK: u32 = 1 << 27;
const CSR_INT_ALIVE: u32 = 1;
const CSR_INT_HW_ERR: u32 = 1 << 29;
const CSR_INT_RX_PERIODIC: u32 = 1 << 28;
const CSR_INT_FH_TX: u32 = 1 << 27;
const CSR_INT_SW_ERR: u32 = 1 << 25;
const CSR_INT_RF_KILL: u32 = 1 << 7;
const CSR_INT_SW_RX: u32 = 1 << 3;
const CSR_INT_FH_RX: u32 = 1 << 31;
const CSR_FH_RX_MASK: u32 = (1 << 30) | (1 << 17) | (1 << 16);
const CSR_FH_TX_MASK: u32 = (1 << 1) | 1;
const CSR_INT_PERIODIC_REG: u32 = 0x005;
const CSR_INT_PERIODIC_DISABLED: u8 = 0;
const CSR_INT_PERIODIC_ENABLED: u8 = 0xff;
const MSIX_FH_Q0: u32 = 1;
const MSIX_FH_Q1: u32 = 1 << 1;
const MSIX_FH_D2S_CH0: u32 = 1 << 16;
const MSIX_FH_ERR: u32 = 1 << 21;
const MSIX_HW_FATAL: u32 = 1 << 3;
const MSIX_HW_SW_ERR_V2: u32 = 1 << 5;
const MSIX_HW_RF_KILL: u32 = 1 << 7;
const MSIX_HW_SW_ERR: u32 = 1 << 25;
const MSIX_HW_HW_ERR: u32 = 1 << 29;
const CSR_MSIX_FH_CAUSES: u32 = 0x2800;
const CSR_MSIX_HW_CAUSES: u32 = 0x2808;
const CSR_MSIX_AUTOMASK: u32 = 0x2810;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IctError {
    Dma(DmaError),
    InvalidSize,
}

impl From<DmaError> for IctError {
    fn from(error: DmaError) -> Self {
        Self::Dma(error)
    }
}

/// The 4-KiB, 4-KiB-aligned interrupt-cause table.
pub struct InterruptCauseTable<R: DmaRegion> {
    pub memory: R,
    pub current: usize,
}

impl<R: DmaRegion> InterruptCauseTable<R> {
    /// Consume pending entries, clear them for reuse, and merge hardware causes.
    // upstream: if_iwx.c iwx_intr() ICT drain and cause bit swizzle
    pub fn drain(&mut self) -> Result<Option<u32>, IctError> {
        if self.memory.capacity() < ICT_SIZE_BYTES || self.current >= ICT_ENTRY_COUNT {
            return Err(IctError::InvalidSize);
        }
        let first = read_entry(&self.memory, self.current)?;
        if first == 0 {
            return Ok(None);
        }
        let mut causes = 0u32;
        let mut current = first;
        while current != 0 {
            causes |= current;
            self.memory.write_at(self.current * 4, &[0, 0, 0, 0])?;
            self.current = (self.current + 1) % ICT_ENTRY_COUNT;
            current = read_entry(&self.memory, self.current)?;
        }
        if causes == u32::MAX {
            causes = 0;
        }
        if causes & 0x000c_0000 != 0 {
            causes |= 0x0000_8000;
        }
        causes = (causes & 0xff) | ((causes & 0xff00) << 16);
        Ok(Some(causes))
    }
}

/// Allocate/clear ICT memory and arm the hardware table pointer.
// upstream: if_iwx.c iwx_ict_reset()
pub fn reset_ict<A, B>(
    allocator: &mut A,
    registers: &mut IwxRegisters<B>,
    masks: &mut InterruptMasks,
    mut table: Option<InterruptCauseTable<A::Region>>,
) -> Result<InterruptCauseTable<A::Region>, IctError>
where
    A: DmaAllocator,
    A::Region: DmaRegion,
    B: CsrAccess,
{
    disable_interrupts(registers, masks);
    let mut table = match table.take() {
        Some(table) if table.memory.capacity() >= ICT_SIZE_BYTES => table,
        Some(_) => return Err(IctError::InvalidSize),
        None => InterruptCauseTable {
            memory: allocator.allocate_aligned(ICT_SIZE_BYTES, ICT_SIZE_BYTES)?,
            current: 0,
        },
    };
    if table.memory.device_address() & ((1 << ICT_ADDRESS_SHIFT) - 1) != 0 {
        return Err(IctError::InvalidSize);
    }
    let mut zeros = Vec::new();
    zeros
        .try_reserve_exact(ICT_SIZE_BYTES)
        .map_err(|_| IctError::Dma(DmaError::AllocationFailed))?;
    zeros.resize(ICT_SIZE_BYTES, 0);
    table.memory.write_at(0, &zeros)?;
    table.current = 0;
    registers.write_csr(
        CSR_DRAM_INT_TBL,
        CSR_DRAM_INT_TBL_ENABLE
            | CSR_DRAM_INT_TBL_WRAP_CHECK
            | CSR_DRAM_INT_TBL_WRITE_POINTER
            | ((table.memory.device_address() >> ICT_ADDRESS_SHIFT) as u32),
    );
    registers.write_csr(CSR_INT, !0);
    enable_interrupts(registers, masks);
    Ok(table)
}

fn read_entry<R: DmaRegion>(memory: &R, index: usize) -> Result<u32, IctError> {
    let mut raw = [0; 4];
    memory.read_at(index * 4, &mut raw)?;
    Ok(u32::from_le_bytes(raw))
}

/// Results the legacy ISR performs after reading and masking cause registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyInterruptWork {
    pub acknowledge: u32,
    pub acknowledge_flow_handler: u32,
    pub firmware_alive: bool,
    pub firmware_chunk: bool,
    pub rfkill: bool,
    pub software_error: bool,
    pub hardware_error: bool,
    pub receive_notifications: bool,
    pub periodic_one_shot: bool,
    pub reenable_periodic: bool,
    pub restore_interrupts: bool,
    pub claimed: bool,
}

/// Translate host/FH status into legacy ISR actions, preserving early-outs.
// upstream: if_iwx.c iwx_intr() status/cause handling
pub fn plan_legacy_interrupt(
    mut host_status: u32,
    flow_status: u32,
    interrupt_mask: u32,
    use_ict: bool,
) -> Option<LegacyInterruptWork> {
    if !use_ict && (host_status == u32::MAX || host_status & 0xffff_fff0 == 0xa5a5_a5a0) {
        return None;
    }
    if host_status == 0 && flow_status == 0 {
        return None;
    }
    let acknowledge = host_status | !interrupt_mask;
    let firmware_alive = host_status & CSR_INT_ALIVE != 0;
    let rfkill = host_status & CSR_INT_RF_KILL != 0;
    if rfkill {
        return Some(LegacyInterruptWork {
            acknowledge,
            acknowledge_flow_handler: 0,
            firmware_alive,
            firmware_chunk: false,
            rfkill: true,
            software_error: false,
            hardware_error: false,
            receive_notifications: false,
            periodic_one_shot: false,
            reenable_periodic: false,
            restore_interrupts: true,
            claimed: true,
        });
    }
    let software_error = host_status & CSR_INT_SW_ERR != 0;
    if software_error {
        return Some(LegacyInterruptWork {
            acknowledge,
            acknowledge_flow_handler: 0,
            firmware_alive,
            firmware_chunk: false,
            rfkill: false,
            software_error: true,
            hardware_error: false,
            receive_notifications: false,
            periodic_one_shot: false,
            reenable_periodic: false,
            restore_interrupts: false,
            claimed: true,
        });
    }
    let hardware_error = host_status & CSR_INT_HW_ERR != 0;
    if hardware_error {
        return Some(LegacyInterruptWork {
            acknowledge,
            acknowledge_flow_handler: 0,
            firmware_alive,
            firmware_chunk: false,
            rfkill: false,
            software_error: false,
            hardware_error: true,
            receive_notifications: false,
            periodic_one_shot: false,
            reenable_periodic: false,
            restore_interrupts: false,
            claimed: true,
        });
    }
    let firmware_chunk = host_status & CSR_INT_FH_TX != 0;
    let receive = host_status & (CSR_INT_FH_RX | CSR_INT_SW_RX | CSR_INT_RX_PERIODIC) != 0;
    let mut acknowledge_flow_handler = 0;
    if host_status & CSR_INT_FH_TX != 0 {
        acknowledge_flow_handler |= CSR_FH_TX_MASK;
    }
    if host_status & (CSR_INT_FH_RX | CSR_INT_SW_RX) != 0 {
        acknowledge_flow_handler |= CSR_FH_RX_MASK;
    }
    let periodic_one_shot = receive;
    let _ = &mut host_status;
    Some(LegacyInterruptWork {
        acknowledge,
        acknowledge_flow_handler,
        firmware_alive,
        firmware_chunk,
        rfkill: false,
        software_error: false,
        hardware_error: false,
        receive_notifications: receive,
        periodic_one_shot,
        reenable_periodic: host_status & (CSR_INT_FH_RX | CSR_INT_SW_RX) != 0,
        restore_interrupts: true,
        claimed: true,
    })
}

/// Results the MSI-X ISR performs after cause/mask intersection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MsixInterruptWork {
    pub acknowledge_fh: u32,
    pub acknowledge_hw: u32,
    pub receive_notifications: bool,
    pub firmware_chunk: bool,
    pub firmware_alive: bool,
    pub rfkill: bool,
    pub software_error: bool,
    pub hardware_error: bool,
    pub top_fatal_error: bool,
    pub reenable_vector: bool,
    pub automask_write: u32,
    pub claimed: bool,
}

/// Translate MSI-X cause registers through configured unmasked cause masks.
// upstream: if_iwx.c iwx_intr_msix()
pub fn plan_msix_interrupt(
    flow_causes: u32,
    hardware_causes: u32,
    masks: &InterruptMasks,
) -> MsixInterruptWork {
    let flow = flow_causes & masks.fh_mask;
    let hardware = hardware_causes & masks.hw_mask;
    let top_fatal_error = hardware & MSIX_HW_FATAL != 0;
    let software_error =
        flow & MSIX_FH_ERR != 0 || hardware & (MSIX_HW_SW_ERR | MSIX_HW_SW_ERR_V2) != 0;
    let hardware_error = hardware & MSIX_HW_HW_ERR != 0;
    let early_fatal = top_fatal_error || software_error || hardware_error;
    MsixInterruptWork {
        acknowledge_fh: flow_causes,
        acknowledge_hw: hardware_causes,
        receive_notifications: flow & (MSIX_FH_Q0 | MSIX_FH_Q1) != 0,
        firmware_chunk: flow & MSIX_FH_D2S_CH0 != 0,
        firmware_alive: hardware & 1 != 0,
        rfkill: hardware & MSIX_HW_RF_KILL != 0,
        software_error,
        hardware_error,
        top_fatal_error,
        reenable_vector: !early_fatal,
        automask_write: u32::from(!early_fatal),
        claimed: flow != 0 || hardware != 0,
    }
}

/// Read/ack causes, apply work policy, and perform legacy periodic/mask writes.
// upstream: if_iwx.c iwx_intr() register read/ack/restore sequence
pub fn service_legacy_interrupt<B: CsrAccess, R: DmaRegion>(
    registers: &mut IwxRegisters<B>,
    masks: &InterruptMasks,
    ict: Option<&mut InterruptCauseTable<R>>,
) -> Result<Option<LegacyInterruptWork>, IctError> {
    let use_ict = ict.is_some();
    let (host_status, flow_status) = if let Some(ict) = ict {
        let Some(causes) = ict.drain()? else {
            registers.write_csr(CSR_INT_MASK, masks.interrupt_mask);
            return Ok(None);
        };
        (causes, 0)
    } else {
        let host_status = registers.read_csr(CSR_INT);
        if host_status == u32::MAX || host_status & 0xffff_fff0 == 0xa5a5_a5a0 {
            return Ok(None);
        }
        (host_status, registers.read_csr(CSR_FH_INT_STATUS))
    };
    let Some(work) = plan_legacy_interrupt(host_status, flow_status, masks.interrupt_mask, use_ict)
    else {
        return Ok(None);
    };
    registers.write_csr(CSR_INT, work.acknowledge);
    if work.acknowledge_flow_handler != 0 {
        registers.write_csr(CSR_FH_INT_STATUS, work.acknowledge_flow_handler);
    }
    if work.periodic_one_shot {
        registers.write_csr8(CSR_INT_PERIODIC_REG, CSR_INT_PERIODIC_DISABLED);
    }
    if work.reenable_periodic {
        registers.write_csr8(CSR_INT_PERIODIC_REG, CSR_INT_PERIODIC_ENABLED);
    }
    if work.restore_interrupts {
        registers.write_csr(CSR_INT_MASK, masks.interrupt_mask);
    }
    Ok(Some(work))
}

/// Read/ack cause registers and re-enable the automasked vector when nonfatal.
// upstream: if_iwx.c iwx_intr_msix() register read/ack/automask sequence
pub fn service_msix_interrupt<B: CsrAccess>(
    registers: &mut IwxRegisters<B>,
    masks: &InterruptMasks,
) -> MsixInterruptWork {
    let flow = registers.read_csr(CSR_MSIX_FH_CAUSES);
    let hardware = registers.read_csr(CSR_MSIX_HW_CAUSES);
    registers.write_csr(CSR_MSIX_FH_CAUSES, flow);
    registers.write_csr(CSR_MSIX_HW_CAUSES, hardware);
    let work = plan_msix_interrupt(flow, hardware, masks);
    if work.reenable_vector {
        registers.write_csr(CSR_MSIX_AUTOMASK, 1);
    }
    work
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::cell::Cell;

    use super::*;
    use crate::{DeviceFamily, IoBarrier};

    struct Region {
        address: u64,
        bytes: Vec<u8>,
    }
    impl DmaRegion for Region {
        fn device_address(&self) -> u64 {
            self.address
        }
        fn capacity(&self) -> usize {
            self.bytes.len()
        }
        fn write(&mut self, data: &[u8]) -> Result<(), DmaError> {
            self.bytes.copy_from_slice(data);
            Ok(())
        }
        fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<(), DmaError> {
            self.bytes
                .get_mut(offset..offset + data.len())
                .ok_or(DmaError::RegionTooSmall)?
                .copy_from_slice(data);
            Ok(())
        }
        fn read_at(&self, offset: usize, data: &mut [u8]) -> Result<(), DmaError> {
            data.copy_from_slice(
                self.bytes
                    .get(offset..offset + data.len())
                    .ok_or(DmaError::RegionTooSmall)?,
            );
            Ok(())
        }
    }
    struct Allocator(Cell<u64>);
    impl DmaAllocator for Allocator {
        type Region = Region;
        fn allocate(&mut self, size: usize) -> Result<Region, DmaError> {
            let address = self.0.get();
            self.0.set(address + 0x1000);
            Ok(Region {
                address,
                bytes: vec![0; size],
            })
        }
    }
    #[derive(Default)]
    struct Bus(Vec<(u32, u32)>);
    impl CsrAccess for Bus {
        fn read32(&mut self, _: u32) -> u32 {
            0
        }
        fn write32(&mut self, offset: u32, value: u32) {
            self.0.push((offset, value));
        }
        fn write8(&mut self, offset: u32, value: u8) {
            self.0.push((offset, u32::from(value)));
        }
        fn barrier(&mut self, _: IoBarrier) {}
        fn delay_us(&mut self, _: u32) {}
    }

    #[test]
    fn ict_reset_zeros_table_and_programs_aligned_device_pointer() {
        let mut allocator = Allocator(Cell::new(0x4000));
        let mut registers = IwxRegisters::new(Bus::default(), DeviceFamily::Ax210, 0);
        let mut masks = InterruptMasks::default();
        let ict =
            reset_ict::<Allocator, _>(&mut allocator, &mut registers, &mut masks, None).unwrap();
        assert_eq!(ict.memory.device_address(), 0x4000);
        assert_eq!(ict.current, 0);
        let writes = registers.into_inner().0;
        assert!(writes.contains(&(CSR_INT, u32::MAX)));
        assert_eq!(writes.last().unwrap().0, CSR_INT_MASK);
    }

    #[test]
    fn ict_merge_clears_entries_wraps_and_applies_source_swizzle() {
        let mut bytes = vec![0; ICT_SIZE_BYTES];
        let mut entry = |index: usize, value: u32| {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        };
        entry(1023, 0x0004_0001);
        entry(0, 0x0000_0002);
        let mut ict = InterruptCauseTable {
            memory: Region {
                address: 0x1000,
                bytes,
            },
            current: 1023,
        };
        assert_eq!(ict.drain().unwrap(), Some(0x8000_0003));
        assert_eq!(ict.current, 1);
        assert_eq!(ict.drain().unwrap(), None);
    }

    #[test]
    fn legacy_interrupt_plan_preserves_rfkill_and_fatal_early_paths() {
        let mask = u32::MAX;
        let rfkill =
            plan_legacy_interrupt(CSR_INT_RF_KILL | CSR_INT_ALIVE, 0, mask, false).unwrap();
        assert!(rfkill.rfkill && rfkill.firmware_alive);
        assert!(rfkill.restore_interrupts && !rfkill.receive_notifications);
        let error = plan_legacy_interrupt(CSR_INT_SW_ERR, 0, mask, false).unwrap();
        assert!(error.software_error && !error.restore_interrupts);
        assert_eq!(plan_legacy_interrupt(u32::MAX, 0, mask, false), None);
    }

    #[test]
    fn msix_interrupt_plan_filters_unmasked_causes_and_preserves_w1c_ack() {
        let masks = InterruptMasks {
            msix: true,
            fh_mask: MSIX_FH_Q0 | MSIX_FH_D2S_CH0,
            hw_mask: MSIX_HW_RF_KILL | 1,
            ..InterruptMasks::default()
        };
        let work = plan_msix_interrupt(MSIX_FH_Q0 | MSIX_FH_D2S_CH0, MSIX_HW_RF_KILL | 1, &masks);
        assert!(
            work.receive_notifications && work.firmware_chunk && work.firmware_alive && work.rfkill
        );
        assert!(work.reenable_vector && work.claimed);
        assert_eq!(work.acknowledge_fh, MSIX_FH_Q0 | MSIX_FH_D2S_CH0);
    }
}
