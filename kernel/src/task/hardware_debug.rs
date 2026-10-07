//! Task-owned x86 user comparators. Perf and ptrace leases exclude each other
//! globally until a shared physical-slot allocator exists.
use axerrno::{AxError, AxResult};
use core::sync::atomic::{AtomicU64, Ordering};
use axhal::uspace::HardwareDebugRegisters;
static OWNERS: AtomicU64 = AtomicU64::new(0);
pub(crate) struct Lease { increment: u64 }
impl Lease {
    pub(crate) fn acquire(ptrace: bool) -> AxResult<Self> {
        let (increment, incompatible) = if ptrace { (1u64 << 32, 0xffff_ffff) } else { (1, !0xffff_ffffu64) };
        OWNERS.try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
            (old & incompatible == 0).then(|| old.checked_add(increment)).flatten()
        }).map_err(|_| AxError::from(axerrno::LinuxError::EBUSY))?;
        Ok(Self { increment })
    }
}
impl Drop for Lease { fn drop(&mut self) { OWNERS.fetch_sub(self.increment, Ordering::AcqRel); } }

#[derive(Default)]
pub(crate) struct DebugState {
    addresses: [u64; 4],
    staged: u8,
    dr6: u64,
    dr7: u64,
    lease: Option<Lease>,
}
impl DebugState {
    pub(crate) fn read(&self, slot: usize) -> u64 {
        match slot { 0..=3 => self.addresses[slot], 6 => self.dr6 ^ 0xffff0ff0, 7 => self.dr7, _ => 0 }
    }
    pub(crate) fn write(&mut self, slot: usize, value: u64) -> AxResult<()> {
        let mut addresses = self.addresses;
        let mut staged = self.staged;
        let mut control = self.dr7;
        match slot {
            0..=3 => { addresses[slot] = value; staged |= 1 << slot; }
            6 => { self.dr6 = value ^ 0xffff0ff0; return Ok(()); }
            7 => control = value,
            _ => return Err(axerrno::LinuxError::EIO.into()),
        }
        validate(addresses, staged, control)?;
        if control & 0xff != 0 && self.lease.is_none() { self.lease = Some(Lease::acquire(true)?); }
        if control & 0xff == 0 { self.lease = None; }
        self.addresses = addresses; self.staged = staged; self.dr7 = control;
        Ok(())
    }
    pub(crate) fn image(&self) -> HardwareDebugRegisters {
        HardwareDebugRegisters { addresses: self.addresses, control: self.dr7 & !0xffff_ffff_0000_fc00 }
    }
    pub(crate) fn trap(&mut self, status: u64) -> Option<usize> {
        self.dr6 = status & ((1 << 14) | 0xf);
        (0..4).find(|slot| status & (1 << slot) != 0 && self.dr7 & (3 << (slot * 2)) != 0)
            .map(|slot| self.addresses[slot] as usize)
    }
}
fn validate(addresses: [u64;4], staged: u8, control: u64) -> AxResult<()> {
    for (slot, address) in addresses.into_iter().enumerate() {
        let enabled = control & (3 << (slot * 2)) != 0;
        if staged & (1 << slot) == 0 && !enabled { continue; }
        let kind = (control >> (16 + slot * 4)) & 3;
        let len = [1,2,8,4][((control >> (18 + slot * 4)) & 3) as usize];
        if kind == 2 || (kind == 0 && len != 1) || address & (len - 1) != 0 {
            return Err(AxError::InvalidInput);
        }
        if address.checked_add(len - 1).is_none_or(|end| end >= 0x8000_0000_0000) {
            return Err(AxError::OperationNotPermitted);
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn debug_validation_and_virtual_status() {
        let mut state = DebugState::default();
        assert_eq!(state.read(6), 0xffff0ff0);
        state.write(0, 0x1000).unwrap(); state.write(7, 0xd0001).unwrap();
        assert_eq!(state.trap(1), Some(0x1000)); assert_eq!(state.read(6), 0xffff0ff1);
        assert!(state.write(0, 0x1001).is_err()); assert_eq!(state.read(0), 0x1000);
        assert!(state.write(7, 0x20001).is_err());
        assert!(Lease::acquire(false).is_err());
        state.write(7, 0).unwrap(); let perf = Lease::acquire(false).unwrap();
        assert!(state.write(7, 0xd0001).is_err()); drop(perf);
        state.write(7, 0xffff_ffff_0000_fc00).unwrap(); assert_eq!(state.image().control, 0);
        assert!(state.write(4, 0).is_err());
    }
}
