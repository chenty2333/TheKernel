//! CSR, peripheral-register and target-memory access from OpenBSD iwx.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230, functions
//! `iwx_prph_addr_mask()`, `iwx_read/write_prph*()`, `iwx_read/write_umac_prph*()`,
//! `iwx_read/write_mem*()`, `iwx_poll_bit()`, `iwx_nic_lock/unlock()` and
//! peripheral bit helpers; CSR/HBUS offsets in `if_iwxreg.h`. ISC.
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>
//!   Author: Stefan Sperling <stsp@openbsd.org>
//! Copyright (c) 2014 Fixup Software Ltd.
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>

const CSR_GP_CNTRL: u32 = 0x024;
const CSR_GP_MAC_CLOCK_READY: u32 = 0x0000_0001;
const CSR_GP_MAC_ACCESS_REQ: u32 = 0x0000_0008;
const CSR_GP_GOING_TO_SLEEP: u32 = 0x0000_0010;
const CSR_GP_MAC_ACCESS_EN: u32 = 0x0000_0001;
const CSR_GP_MAC_STATUS: u32 = 1 << 20;
const CSR_GP_BZ_MAC_ACCESS_REQ: u32 = 1 << 21;
const HBUS_BASE: u32 = 0x400;
const HBUS_TARG_MEM_RADDR: u32 = HBUS_BASE + 0x00c;
const HBUS_TARG_MEM_WADDR: u32 = HBUS_BASE + 0x010;
const HBUS_TARG_MEM_WDAT: u32 = HBUS_BASE + 0x018;
const HBUS_TARG_MEM_RDAT: u32 = HBUS_BASE + 0x01c;
const HBUS_TARG_PRPH_WADDR: u32 = HBUS_BASE + 0x044;
const HBUS_TARG_PRPH_RADDR: u32 = HBUS_BASE + 0x048;
const HBUS_TARG_PRPH_WDAT: u32 = HBUS_BASE + 0x04c;
const HBUS_TARG_PRPH_RDAT: u32 = HBUS_BASE + 0x050;
const PRPH_READ_MODE: u32 = 3 << 24;

/// CSR aperture access and delay interface supplied by the PCI platform layer.
pub trait CsrAccess {
    fn read32(&mut self, offset: u32) -> u32;
    fn write32(&mut self, offset: u32, value: u32);
    fn barrier(&mut self, direction: IoBarrier);
    fn delay_us(&mut self, micros: u32);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoBarrier {
    ReadWrite,
    Write,
}

/// Device generation information used by PRPH addressing and NIC arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum DeviceFamily {
    Legacy      = 0,
    Family22000 = 1,
    Ax210       = 2,
    Bz          = 3,
}

/// Register-layer failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    Busy,
    NotLocked,
}

/// Serialized CSR/HBUS access and reentrant NIC-access ownership.
pub struct IwxRegisters<B: CsrAccess> {
    bus: B,
    family: DeviceFamily,
    umac_prph_offset: u32,
    nic_locks: u32,
}

impl<B: CsrAccess> IwxRegisters<B> {
    pub const fn new(bus: B, family: DeviceFamily, umac_prph_offset: u32) -> Self {
        Self {
            bus,
            family,
            umac_prph_offset,
            nic_locks: 0,
        }
    }

    pub fn into_inner(self) -> B {
        self.bus
    }
    pub const fn family(&self) -> DeviceFamily {
        self.family
    }
    pub const fn nic_lock_count(&self) -> u32 {
        self.nic_locks
    }

    /// Read a CSR register directly through the PCI BAR.
    pub fn read_csr(&mut self, offset: u32) -> u32 {
        self.bus.read32(offset)
    }

    /// Write a CSR register directly through the PCI BAR.
    pub fn write_csr(&mut self, offset: u32, value: u32) {
        self.bus.write32(offset, value)
    }

    /// Delay through the platform-provided MMIO timing source.
    pub fn delay_us(&mut self, micros: u32) {
        self.bus.delay_us(micros)
    }

    pub fn set_csr_bits(&mut self, offset: u32, bits: u32) {
        let value = self.bus.read32(offset);
        self.bus.write32(offset, value | bits);
    }

    pub fn clear_csr_bits(&mut self, offset: u32, bits: u32) {
        let value = self.bus.read32(offset);
        self.bus.write32(offset, value & !bits);
    }

    /// The PRPH address width changes at AX210.
    // upstream: if_iwx.c iwx_prph_addr_mask()
    pub const fn prph_addr_mask(&self) -> u32 {
        Self::prph_addr_mask_for(self.family)
    }

    pub const fn prph_addr_mask_for(family: DeviceFamily) -> u32 {
        if (family as u8) >= DeviceFamily::Ax210 as u8 {
            0x00ff_ffff
        } else {
            0x000f_ffff
        }
    }

    /// Acquire MAC/PRPH access, allowing nested calls by the same serialized owner.
    // upstream: if_iwx.c iwx_nic_lock()
    pub fn nic_lock(&mut self) -> Result<(), RegisterError> {
        if self.nic_locks != 0 {
            self.nic_locks = self.nic_locks.checked_add(1).ok_or(RegisterError::Busy)?;
            return Ok(());
        }
        let (access_req, ready, mask) = if self.family >= DeviceFamily::Bz {
            (
                CSR_GP_BZ_MAC_ACCESS_REQ,
                CSR_GP_MAC_STATUS,
                CSR_GP_MAC_STATUS,
            )
        } else {
            (
                CSR_GP_MAC_ACCESS_REQ,
                CSR_GP_MAC_ACCESS_EN,
                CSR_GP_MAC_CLOCK_READY | CSR_GP_GOING_TO_SLEEP,
            )
        };
        let control = self.bus.read32(CSR_GP_CNTRL);
        self.bus.write32(CSR_GP_CNTRL, control | access_req);
        self.bus.delay_us(2);
        if self.poll_bit(CSR_GP_CNTRL, ready, mask, 150_000) {
            self.nic_locks = 1;
            Ok(())
        } else {
            Err(RegisterError::Busy)
        }
    }

    /// Release one nested level of MAC/PRPH ownership.
    // upstream: if_iwx.c iwx_nic_unlock()
    pub fn nic_unlock(&mut self) -> bool {
        if self.nic_locks == 0 {
            return false;
        }
        self.nic_locks -= 1;
        if self.nic_locks == 0 {
            let request = if self.family >= DeviceFamily::Bz {
                CSR_GP_BZ_MAC_ACCESS_REQ
            } else {
                CSR_GP_MAC_ACCESS_REQ
            };
            let control = self.bus.read32(CSR_GP_CNTRL);
            self.bus.write32(CSR_GP_CNTRL, control & !request);
        }
        true
    }

    /// Poll every ten microseconds, checking once even for a short timeout.
    // upstream: if_iwx.c iwx_poll_bit()
    pub fn poll_bit(&mut self, register: u32, bits: u32, mask: u32, timeout_us: u32) -> bool {
        let mut remaining = timeout_us;
        loop {
            if self.bus.read32(register) & mask == bits & mask {
                return true;
            }
            if remaining < 10 {
                return false;
            }
            remaining -= 10;
            self.bus.delay_us(10);
        }
    }

    /// Read an internal peripheral register without acquiring NIC ownership.
    // upstream: if_iwx.c iwx_read_prph_unlocked()
    pub fn read_prph_unlocked(&mut self, address: u32) -> u32 {
        let address = (address & self.prph_addr_mask()) | PRPH_READ_MODE;
        self.bus.write32(HBUS_TARG_PRPH_RADDR, address);
        self.bus.barrier(IoBarrier::ReadWrite);
        self.bus.read32(HBUS_TARG_PRPH_RDAT)
    }

    /// Read an internal peripheral register while NIC ownership is held.
    // upstream: if_iwx.c iwx_read_prph()
    pub fn read_prph(&mut self, address: u32) -> Result<u32, RegisterError> {
        if self.nic_locks == 0 {
            return Err(RegisterError::NotLocked);
        }
        Ok(self.read_prph_unlocked(address))
    }

    /// Write an internal peripheral register without acquiring NIC ownership.
    // upstream: if_iwx.c iwx_write_prph_unlocked()
    pub fn write_prph_unlocked(&mut self, address: u32, value: u32) {
        self.bus.write32(
            HBUS_TARG_PRPH_WADDR,
            (address & self.prph_addr_mask()) | PRPH_READ_MODE,
        );
        self.bus.barrier(IoBarrier::Write);
        self.bus.write32(HBUS_TARG_PRPH_WDAT, value);
    }

    /// Write an internal peripheral register while NIC ownership is held.
    // upstream: if_iwx.c iwx_write_prph()
    pub fn write_prph(&mut self, address: u32, value: u32) -> Result<(), RegisterError> {
        if self.nic_locks == 0 {
            return Err(RegisterError::NotLocked);
        }
        self.write_prph_unlocked(address, value);
        Ok(())
    }

    // upstream: if_iwx.c iwx_read_umac_prph_unlocked()
    pub fn read_umac_prph_unlocked(&mut self, address: u32) -> u32 {
        self.read_prph_unlocked(address + self.umac_prph_offset)
    }

    // upstream: if_iwx.c iwx_read_umac_prph()
    pub fn read_umac_prph(&mut self, address: u32) -> Result<u32, RegisterError> {
        self.read_prph(address + self.umac_prph_offset)
    }

    // upstream: if_iwx.c iwx_write_umac_prph_unlocked()
    pub fn write_umac_prph_unlocked(&mut self, address: u32, value: u32) {
        self.write_prph_unlocked(address + self.umac_prph_offset, value)
    }

    // upstream: if_iwx.c iwx_write_umac_prph()
    pub fn write_umac_prph(&mut self, address: u32, value: u32) -> Result<(), RegisterError> {
        self.write_prph(address + self.umac_prph_offset, value)
    }

    /// Read words from target memory with the auto-incrementing HBUS data register.
    // upstream: if_iwx.c iwx_read_mem()
    pub fn read_mem(&mut self, address: u32, words: &mut [u32]) -> Result<(), RegisterError> {
        self.nic_lock()?;
        self.bus.write32(HBUS_TARG_MEM_RADDR, address);
        for word in words {
            *word = u32::from_le(self.bus.read32(HBUS_TARG_MEM_RDAT));
        }
        self.nic_unlock();
        Ok(())
    }

    /// Write words to target memory; a missing input is represented by `None` and writes zeroes.
    // upstream: if_iwx.c iwx_write_mem()
    pub fn write_mem(
        &mut self,
        address: u32,
        words: Option<&[u32]>,
        count: usize,
    ) -> Result<(), RegisterError> {
        self.nic_lock()?;
        self.bus.write32(HBUS_TARG_MEM_WADDR, address);
        for index in 0..count {
            let word = words
                .and_then(|values| values.get(index))
                .copied()
                .unwrap_or(0);
            self.bus.write32(HBUS_TARG_MEM_WDAT, word.to_le());
        }
        self.nic_unlock();
        Ok(())
    }

    // upstream: if_iwx.c iwx_write_mem32()
    pub fn write_mem32(&mut self, address: u32, value: u32) -> Result<(), RegisterError> {
        self.write_mem(address, Some(core::slice::from_ref(&value)), 1)
    }

    // upstream: if_iwx.c iwx_set_bits_mask_prph()
    pub fn set_bits_mask_prph(
        &mut self,
        register: u32,
        bits: u32,
        mask: u32,
    ) -> Result<(), RegisterError> {
        self.nic_lock()?;
        let current = self.read_prph(register)? & mask;
        self.write_prph(register, current | bits)?;
        self.nic_unlock();
        Ok(())
    }

    // upstream: if_iwx.c iwx_set_bits_prph()
    pub fn set_bits_prph(&mut self, register: u32, bits: u32) -> Result<(), RegisterError> {
        self.set_bits_mask_prph(register, bits, u32::MAX)
    }

    // upstream: if_iwx.c iwx_clear_bits_prph()
    pub fn clear_bits_prph(&mut self, register: u32, bits: u32) -> Result<(), RegisterError> {
        self.set_bits_mask_prph(register, 0, !bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prph_address_width_changes_at_ax210() {
        assert_eq!(
            IwxRegisters::<TestBus>::prph_addr_mask_for(DeviceFamily::Legacy),
            0x000f_ffff
        );
        assert_eq!(
            IwxRegisters::<TestBus>::prph_addr_mask_for(DeviceFamily::Family22000),
            0x000f_ffff
        );
        assert_eq!(
            IwxRegisters::<TestBus>::prph_addr_mask_for(DeviceFamily::Ax210),
            0x00ff_ffff
        );
        assert_eq!(
            IwxRegisters::<TestBus>::prph_addr_mask_for(DeviceFamily::Bz),
            0x00ff_ffff
        );
    }

    struct TestBus;
    impl CsrAccess for TestBus {
        fn read32(&mut self, _offset: u32) -> u32 {
            0
        }
        fn write32(&mut self, _offset: u32, _value: u32) {}
        fn barrier(&mut self, _direction: IoBarrier) {}
        fn delay_us(&mut self, _micros: u32) {}
    }
}
