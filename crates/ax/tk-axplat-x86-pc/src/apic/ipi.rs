//! Local register IPI delivery, following TGOSKits x86-apic-driver (#2432).
//!
//! Neither an IPI nor an EOI needs a mutable reference to the shared boot-time
//! `LocalApic` handle. Registers address the executing CPU's controller, so
//! different CPUs may use this path simultaneously without aliasing that handle.

use core::sync::atomic::Ordering;

const ICR_HIGH: usize = 0x310;
const ICR_LOW: usize = 0x300;
const EOI: usize = 0xb0;
const X2APIC_ICR: u32 = 0x830;
const X2APIC_SELF_IPI: u32 = 0x83f;
const X2APIC_EOI: u32 = 0x80b;
const FIXED_ASSERT: u32 = 1 << 14;
const SELF: u32 = 1 << 18;
const OTHERS: u32 = 3 << 18;
const DELIVERY_PENDING: u32 = 1 << 12;
const DELIVERY_SPINS: usize = 1_000_000;

#[derive(Clone, Copy)]
pub(super) enum Destination {
    Apic(u32),
    Current,
    Others,
}

/// Produces architectural register values, not the x2apic crate's shifted-ID
/// API. Reject unrepresentable xAPIC destinations before any register write.
fn command(vector: u8, destination: Destination, x2apic: bool) -> Option<(u32, u32)> {
    let (high, shorthand) = match destination {
        Destination::Apic(id) => (super::encode_lapic_destination(id, x2apic)?, 0),
        Destination::Current => (0, SELF),
        Destination::Others => (0, OTHERS),
    };
    Some((high, FIXED_ASSERT | shorthand | u32::from(vector)))
}

fn write_xapic(
    high: u32,
    low: u32,
    mut read_low: impl FnMut() -> u32,
    mut write: impl FnMut(usize, u32),
) -> bool {
    // Do not overwrite an older command, even if it came from an AP startup
    // path. Keep the two writes locally IRQ-serialized in the caller.
    let mut wait_idle = || {
        for _ in 0..DELIVERY_SPINS {
            if read_low() & DELIVERY_PENDING == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    };
    if !wait_idle() {
        return false;
    }
    write(ICR_HIGH, high);
    write(ICR_LOW, low);
    wait_idle()
}

pub(super) fn send(vector: u8, destination: Destination) {
    // In particular, a timer IRQ must not interleave two xAPIC ICR dwords or
    // move this publisher to another CPU midway through a local command.
    let _guard = kernel_guard::NoPreemptIrqSave::new();
    let x2apic = super::IS_X2APIC.load(Ordering::Acquire);
    let (high, low) = command(vector, destination, x2apic)
        .expect("IPI destination cannot be represented by this APIC");
    if x2apic {
        // x2APIC WRMSR is not serializing. Publish mailbox/TLB stores before
        // the doorbell (also for a self-IPI). No delivery-status poll applies:
        // the x2APIC ICR's delivery-status bit is reserved.
        unsafe {
            core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags));
            if matches!(destination, Destination::Current) {
                x86::msr::wrmsr(X2APIC_SELF_IPI, u64::from(vector));
            } else {
                x86::msr::wrmsr(X2APIC_ICR, (u64::from(high) << 32) | u64::from(low));
            }
        }
    } else {
        let base = axplat::mem::phys_to_virt(super::lapic_mmio_base()).as_usize();
        // SAFETY: platform init mapped this CPU's UC LAPIC aperture. The guard
        // prevents local ICR writers interleaving; x86 orders earlier WB stores
        // before this UC doorbell. No shared LocalApic reference is formed.
        let delivered = write_xapic(
            high,
            low,
            || unsafe { core::ptr::read_volatile((base + ICR_LOW) as *const u32) },
            |offset, value| unsafe {
                core::ptr::write_volatile((base + offset) as *mut u32, value);
            },
        );
        assert!(
            delivered,
            "local APIC did not accept IPI within bounded wait"
        );
    }
}

pub(super) fn end_of_interrupt() {
    // The IRQ entry already pins us to this CPU. EOI is a local register with
    // no software state, so CPUs never need to borrow the boot handle here.
    unsafe {
        if super::IS_X2APIC.load(Ordering::Acquire) {
            x86::msr::wrmsr(X2APIC_EOI, 0);
        } else {
            let base = axplat::mem::phys_to_virt(super::lapic_mmio_base()).as_usize();
            core::ptr::write_volatile((base + EOI) as *mut u32, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, vec::Vec};

    use super::*;

    #[test]
    fn physical_ids_and_shorthands_use_architectural_fields() {
        assert_eq!(
            command(0xf3, Destination::Apic(0x1234_5678), true),
            Some((0x1234_5678, 0x40f3))
        );
        assert_eq!(
            command(0xf3, Destination::Apic(0xff), false),
            Some((0xff00_0000, 0x40f3))
        );
        assert_eq!(command(0xf3, Destination::Apic(0x100), false), None);
        for mode in [false, true] {
            assert_eq!(
                command(0xf3, Destination::Current, mode),
                Some((0, 0x440f3))
            );
            assert_eq!(command(0xf3, Destination::Others, mode), Some((0, 0xc40f3)));
        }
    }

    #[test]
    fn xapic_waits_before_publication_and_after_low_dword() {
        let reads = Cell::new(0);
        let mut writes = Vec::new();
        assert!(write_xapic(
            0x7f00_0000,
            0x40f3,
            || {
                let n = reads.get();
                reads.set(n + 1);
                if n == 0 || n == 2 {
                    DELIVERY_PENDING
                } else {
                    0
                }
            },
            |offset, value| {
                assert_eq!(reads.get(), 2);
                writes.push((offset, value));
            }
        ));
        assert_eq!(writes, [(ICR_HIGH, 0x7f00_0000), (ICR_LOW, 0x40f3)]);
        assert_eq!(reads.get(), 4);
    }

    #[test]
    fn xapic_timeout_does_not_overwrite_pending_command() {
        assert!(!write_xapic(
            0,
            0x40f3,
            || DELIVERY_PENDING,
            |_, _| panic!("must not publish")
        ));
    }

    #[test]
    fn xapic_post_publication_timeout_is_not_reported_as_delivery() {
        let published = Cell::new(false);
        assert!(!write_xapic(
            0,
            0x40f3,
            || { if published.get() { DELIVERY_PENDING } else { 0 } },
            |offset, _| {
                if offset == ICR_LOW {
                    published.set(true);
                }
            }
        ));
        assert!(published.get());
    }
}
