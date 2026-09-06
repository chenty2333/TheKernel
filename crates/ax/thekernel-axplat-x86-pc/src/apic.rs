//! Advanced Programmable Interrupt Controller (APIC) support.

use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use axplat::mem::{PhysAddr, pa, phys_to_virt};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use x2apic::{
    ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry},
    lapic::{LocalApic, LocalApicBuilder, xapic_base},
};
#[cfg(feature = "pmu-sampling")]
use x86::msr::{rdmsr, wrmsr};
use x86_64::instructions::port::Port;

use self::vectors::*;

pub(super) mod vectors {
    /// First vector reserved for local-APIC-only uses. PMU overflow uses
    /// delivery-mode NMI (architectural vector 2), not this vector field.
    pub const APIC_LOCAL_RESERVED_VECTOR: u8 = 0xef;
    pub const APIC_TIMER_VECTOR: u8 = 0xf0;
    pub const APIC_SPURIOUS_VECTOR: u8 = 0xf1;
    pub const APIC_ERROR_VECTOR: u8 = 0xf2;
}

const IO_APIC_BASE: PhysAddr = pa!(0xFEC0_0000);
const IO_APIC_VECTOR_BASE: usize = 0x20;
#[cfg(feature = "pmu-sampling")]
const XAPIC_LVT_PERF_OFFSET: usize = 0x340;
#[cfg(feature = "pmu-sampling")]
const X2APIC_LVT_PERF_MSR: u32 = 0x834;

static mut LOCAL_APIC: MaybeUninit<LocalApic> = MaybeUninit::uninit();
static IS_X2APIC: AtomicBool = AtomicBool::new(false);
static IO_APIC: LazyInit<SpinNoIrq<IoApic>> = LazyInit::new();
const IO_APIC_DEST_UNAVAILABLE: u32 = u32::MAX;
static IO_APIC_DEST: AtomicU32 = AtomicU32::new(IO_APIC_DEST_UNAVAILABLE);

/// Enables or disables the given IRQ.
#[cfg(feature = "irq")]
pub fn set_enable(vector: usize, enabled: bool) {
    let Some(pin) = io_apic_pin(vector) else {
        // LAPIC vectors (including the timer and IPIs) do not have an IOAPIC
        // redirection entry.
        return;
    };
    if enabled && IO_APIC_DEST.load(Ordering::Acquire) == IO_APIC_DEST_UNAVAILABLE {
        // An x2APIC ID wider than the IOAPIC's physical eight-bit destination
        // field cannot be delivered without interrupt remapping.  Keep every
        // line masked rather than allowing a wrapped destination.
        warn!("IOAPIC delivery is unavailable; refusing to unmask IRQ {vector}");
        return;
    }

    unsafe {
        let mut io_apic = IO_APIC.lock();
        // The IOAPIC advertises its implemented pin count at runtime.  Do
        // not touch reserved selectors when a caller supplies an unrelated
        // CPU vector.
        if pin > io_apic.max_table_entry() {
            return;
        }
        if enabled {
            io_apic.enable_irq(pin);
        } else {
            io_apic.disable_irq(pin);
        }
    }
}

/// Configure only an admitted PCI INTx line as level-triggered, active-low.
#[cfg(feature = "irq")]
pub fn configure_pci_intx(vector: usize) -> bool {
    let Some(pin) = io_apic_pin(vector) else {
        return false;
    };
    let destination = IO_APIC_DEST.load(Ordering::Acquire);
    if destination == IO_APIC_DEST_UNAVAILABLE {
        return false;
    }
    unsafe {
        let mut io_apic = IO_APIC.lock();
        if pin > io_apic.max_table_entry() {
            return false;
        }
        let mut entry = io_apic.table_entry(pin);
        if entry.vector() as usize != vector
            || entry.dest() as u32 != destination
            || entry.flags().contains(IrqFlags::LOGICAL_DEST)
        {
            return false;
        }
        if entry
            .flags()
            .contains(IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE)
        {
            // Another device may already be using this shared line.
            return true;
        }
        let was_masked = entry.flags().contains(IrqFlags::MASKED);
        if !was_masked {
            // Change the electrical mode only while delivery is masked.
            io_apic.disable_irq(pin);
        }
        entry.set_flags(entry.flags() | IrqFlags::MASKED);
        set_pci_intx_flags(&mut entry);
        io_apic.set_table_entry(pin, entry);
        if !was_masked {
            io_apic.enable_irq(pin);
        }
    }
    true
}

#[cfg(any(feature = "irq", test))]
fn set_pci_intx_flags(entry: &mut RedirectionTableEntry) {
    entry.set_flags(entry.flags() | IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE);
}

#[cfg(feature = "irq")]
pub use irq_impl::register_shared_dispatcher;

/// Translate the x86 external-interrupt vector range to an IOAPIC pin.
///
/// The common x86 IDT contract starts external vectors at `0x20`, while the
/// IOAPIC redirection table is indexed by the legacy ISA GSI.  Thus COM1's
/// GSI/pin 4 is deliberately delivered as vector `0x24`.
fn io_apic_pin(vector: usize) -> Option<u8> {
    let pin = vector.checked_sub(IO_APIC_VECTOR_BASE)?;
    let mapped_vector = io_apic_vector(pin as u8)?;
    (mapped_vector as usize == vector).then_some(pin as u8)
}

/// Map an IOAPIC pin to an external-interrupt vector without crossing into
/// the LAPIC-reserved vector range or overflowing the u8 vector field.
fn io_apic_vector(pin: u8) -> Option<u8> {
    let vector = IO_APIC_VECTOR_BASE.checked_add(pin as usize)?;
    (vector < APIC_LOCAL_RESERVED_VECTOR as usize && vector <= u8::MAX as usize)
        .then_some(vector as u8)
}

#[cfg(any(feature = "smp", feature = "irq"))]
#[allow(static_mut_refs)]
pub fn local_apic<'a>() -> &'a mut LocalApic {
    // It's safe as `LOCAL_APIC` is initialized in `init_primary`.
    unsafe { LOCAL_APIC.assume_init_mut() }
}

/// Reads this CPU's complete LVT Performance Counter register.
///
/// `x2apic` deliberately does not expose this LVT entry through `LocalApic`.
/// Keep the architectural register access here so PMU code has one local-only
/// implementation for both xAPIC MMIO and x2APIC MSR modes.
#[cfg(feature = "pmu-sampling")]
pub unsafe fn read_lvt_perf() -> u32 {
    if IS_X2APIC.load(Ordering::Acquire) {
        unsafe { rdmsr(X2APIC_LVT_PERF_MSR) as u32 }
    } else {
        let base = phys_to_virt(pa!(unsafe { xapic_base() } as usize));
        unsafe { core::ptr::read_volatile((base.as_usize() + XAPIC_LVT_PERF_OFFSET) as *const u32) }
    }
}

/// Writes this CPU's complete LVT Performance Counter register.
#[cfg(feature = "pmu-sampling")]
pub unsafe fn write_lvt_perf(value: u32) {
    if IS_X2APIC.load(Ordering::Acquire) {
        unsafe {
            wrmsr(X2APIC_LVT_PERF_MSR, value as u64);
        }
    } else {
        let base = phys_to_virt(pa!(unsafe { xapic_base() } as usize));
        unsafe {
            core::ptr::write_volatile((base.as_usize() + XAPIC_LVT_PERF_OFFSET) as *mut u32, value);
        }
    }
}

#[cfg(any(feature = "smp", feature = "irq"))]
pub fn raw_apic_id(apic_id: u32) -> Option<u32> {
    encode_lapic_destination(apic_id, IS_X2APIC.load(Ordering::Acquire))
}

fn cpu_has_x2apic() -> bool {
    super::cpu::x2apic_supported()
}

pub fn init_primary(logical_cpu_id: usize) {
    info!("Initialize Local APIC...");

    unsafe {
        // Disable 8259A interrupt controllers
        Port::<u8>::new(0x21).write(0xff);
        Port::<u8>::new(0xA1).write(0xff);
    }

    let mut builder = LocalApicBuilder::new();
    builder
        .timer_vector(APIC_TIMER_VECTOR as _)
        .error_vector(APIC_ERROR_VECTOR as _)
        .spurious_vector(APIC_SPURIOUS_VECTOR as _);

    if cpu_has_x2apic() {
        info!("Using x2APIC.");
        IS_X2APIC.store(true, Ordering::Release);
    } else {
        info!("Using xAPIC.");
        let base_vaddr = phys_to_virt(pa!(unsafe { xapic_base() } as usize));
        builder.set_xapic_base(base_vaddr.as_usize() as u64);
    }

    let mut lapic = builder.build().unwrap();
    unsafe {
        lapic.enable();
        let bsp_apic_id = normalize_lapic_id(lapic.id());
        super::cpu::assert_current_apic_id(bsp_apic_id);
        assert_eq!(
            super::cpu::logical_cpu_id_for_apic(bsp_apic_id),
            Some(logical_cpu_id),
            "BSP logical CPU ID does not match the APIC topology"
        );
        #[allow(static_mut_refs)]
        LOCAL_APIC.write(lapic);

        info!("BSP logical CPU {logical_cpu_id} uses hardware APIC ID {bsp_apic_id:#x}");
    }

    info!("Initialize IO APIC...");
    let io_apic = unsafe { IoApic::new(phys_to_virt(IO_APIC_BASE).as_usize() as u64) };
    IO_APIC.init_once(SpinNoIrq::new(io_apic));
    init_io_apic(super::cpu::hardware_apic_id());
}

/// Install a safe, deterministic redirection table before any device IRQ is
/// unmasked.  All lines target the BSP and start masked; external lines use
/// fixed, edge-triggered, high-active delivery with the standard `0x20` vector
/// base.  In particular, COM1 is pin/GSI 4 -> CPU vector `0x24`.
fn init_io_apic(bsp_apic_id: u32) {
    let destination = u8::try_from(bsp_apic_id).ok();
    IO_APIC_DEST.store(
        destination.map_or(IO_APIC_DEST_UNAVAILABLE, u32::from),
        Ordering::Release,
    );
    unsafe {
        let mut io_apic = IO_APIC.lock();
        let max_pin = io_apic.max_table_entry();

        // First overwrite every implemented entry with an explicit masked
        // baseline.  This pass is intentionally independent of vector
        // allocation: pins above the usable vector range must not retain any
        // firmware-programmed delivery state.
        for pin in 0..=max_pin {
            let mut entry = RedirectionTableEntry::default();
            entry.set_mode(IrqMode::Fixed);
            entry.set_flags(IrqFlags::MASKED);
            io_apic.set_table_entry(pin, entry);
        }

        // Program only vectors that do not overlap LAPIC-reserved vectors.
        // They remain masked until a handler registration explicitly enables
        // the line.  When the BSP ID does not fit the IOAPIC destination field
        // the entries remain masked and `set_enable` refuses to unmask them.
        for pin in 0..=max_pin {
            let mut entry = RedirectionTableEntry::default();
            entry.set_mode(IrqMode::Fixed);
            entry.set_flags(IrqFlags::MASKED);
            if let Some(vector) = io_apic_vector(pin) {
                entry.set_vector(vector);
                if let Some(destination) = destination {
                    entry.set_dest(destination);
                }
            }
            io_apic.set_table_entry(pin, entry);
        }

        if destination.is_none() {
            warn!(
                "BSP APIC ID {bsp_apic_id:#x} exceeds the IOAPIC 8-bit destination; all IOAPIC \
                 lines remain masked"
            );
        }
    }
}

/// Encode a physical IPI destination for the register layout used by the
/// x2apic crate.  xAPIC stores an eight-bit destination in the high dword;
/// x2APIC stores the complete 32-bit ID.  The xAPIC branch rejects IDs that
/// cannot be represented instead of truncating them.
fn encode_lapic_destination(apic_id: u32, x2apic: bool) -> Option<u32> {
    if x2apic {
        Some(apic_id)
    } else if apic_id <= u8::MAX as u32 {
        Some(apic_id << 24)
    } else {
        None
    }
}

fn normalize_lapic_id(raw_id: u32) -> u32 {
    normalize_lapic_id_for_mode(raw_id, IS_X2APIC.load(Ordering::Acquire))
}

fn normalize_lapic_id_for_mode(raw_id: u32, x2apic: bool) -> u32 {
    if x2apic { raw_id } else { raw_id >> 24 }
}

#[cfg(test)]
mod tests {
    use super::{
        encode_lapic_destination, io_apic_pin, io_apic_vector, normalize_lapic_id_for_mode,
    };

    #[cfg(feature = "irq")]
    #[test]
    fn shared_dispatcher_registration_keeps_one_owner() {
        fn first(_: usize) -> bool {
            true
        }
        fn second(_: usize) -> bool {
            false
        }
        assert!(super::register_shared_dispatcher(first));
        assert!(super::register_shared_dispatcher(first));
        assert!(!super::register_shared_dispatcher(second));
    }

    #[test]
    fn pci_intx_preserves_route_and_mask() {
        use super::{IrqFlags, IrqMode, RedirectionTableEntry, set_pci_intx_flags};
        for masked in [false, true] {
            let mut entry = RedirectionTableEntry::default();
            entry.set_vector(0x2b);
            entry.set_dest(7);
            entry.set_mode(IrqMode::Fixed);
            if masked {
                entry.set_flags(IrqFlags::MASKED);
            }
            set_pci_intx_flags(&mut entry);
            assert_eq!(entry.vector(), 0x2b);
            assert_eq!(entry.dest(), 7);
            assert!(matches!(entry.mode(), IrqMode::Fixed));
            assert_eq!(entry.flags().contains(IrqFlags::MASKED), masked);
            assert!(
                entry
                    .flags()
                    .contains(IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE)
            );
        }
    }

    #[test]
    fn external_vectors_map_to_ioapic_pins() {
        assert_eq!(io_apic_pin(0x20), Some(0));
        assert_eq!(io_apic_pin(0x24), Some(4));
        assert_eq!(io_apic_pin(0x37), Some(0x17));
        assert_eq!(io_apic_pin(0x1f), None);
        assert_eq!(io_apic_pin(0xf0), None);
        assert_eq!(io_apic_pin(0xff), None);
    }

    #[test]
    fn ioapic_vector_mapping_is_bounded() {
        assert_eq!(io_apic_vector(0), Some(0x20));
        assert_eq!(io_apic_vector(0xce), Some(0xee));
        assert_eq!(io_apic_vector(0xcf), None);
        assert_eq!(io_apic_vector(u8::MAX), None);
    }

    #[test]
    fn lapic_destination_preserves_full_x2apic_ids() {
        assert_eq!(
            encode_lapic_destination(0x1234_5678, true),
            Some(0x1234_5678)
        );
        assert_eq!(encode_lapic_destination(0xff, false), Some(0xff00_0000));
        assert_eq!(encode_lapic_destination(0x100, false), None);
    }

    #[test]
    fn xapic_id_normalization_removes_register_shift_only_in_xapic_mode() {
        assert_eq!(normalize_lapic_id_for_mode(0x7f00_0000, false), 0x7f);
        assert_eq!(normalize_lapic_id_for_mode(0x1234_5678, true), 0x1234_5678);
    }
}

#[cfg(feature = "smp")]
pub fn init_secondary(logical_cpu_id: usize) {
    unsafe {
        local_apic().enable();
        let apic_id = normalize_lapic_id(local_apic().id());
        super::cpu::assert_current_apic_id(apic_id);
        assert_eq!(
            super::cpu::logical_cpu_id_for_apic(apic_id),
            Some(logical_cpu_id),
            "secondary logical CPU ID does not match the APIC topology"
        );
    }
}

#[cfg(feature = "irq")]
mod irq_impl {
    use axplat::irq::{HandlerTable, IpiTarget, IrqHandler, IrqIf};

    /// The maximum number of IRQs.
    const MAX_IRQ_COUNT: usize = 256;

    static IRQ_HANDLER_TABLE: HandlerTable<MAX_IRQ_COUNT> = HandlerTable::new();

    static SHARED_DISPATCHER: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);

    /// Install the shared-source acknowledgement pass, before direct handlers.
    pub fn register_shared_dispatcher(dispatcher: fn(usize) -> bool) -> bool {
        use core::sync::atomic::Ordering;
        let address = dispatcher as usize;
        match SHARED_DISPATCHER.compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(existing) => existing == address,
        }
    }

    struct IrqIfImpl;

    #[cfg_attr(target_os = "none", impl_plat_interface)]
    impl IrqIf for IrqIfImpl {
        /// Enables or disables the given IRQ.
        fn set_enable(vector: usize, enabled: bool) {
            super::set_enable(vector, enabled);
        }

        /// Registers an IRQ handler for the given IRQ.
        ///
        /// It also enables the IRQ if the registration succeeds. It returns `false` if
        /// the registration failed.
        fn register(vector: usize, handler: IrqHandler) -> bool {
            if IRQ_HANDLER_TABLE.register_handler(vector, handler) {
                Self::set_enable(vector, true);
                return true;
            }
            warn!("register handler for IRQ {} failed", vector);
            false
        }

        /// Unregisters the IRQ handler for the given IRQ.
        ///
        /// It also disables the IRQ if the unregistration succeeds. It returns the
        /// existing handler if it is registered, `None` otherwise.
        fn unregister(vector: usize) -> Option<IrqHandler> {
            Self::set_enable(vector, false);
            IRQ_HANDLER_TABLE.unregister_handler(vector)
        }

        /// Handles the IRQ.
        ///
        /// It is called by the common interrupt handler. It should look up in the
        /// IRQ handler table and calls the corresponding handler. If necessary, it
        /// also acknowledges the interrupt controller after handling.
        fn handle(vector: usize) -> Option<usize> {
            trace!("IRQ {}", vector);
            let address = SHARED_DISPATCHER.load(core::sync::atomic::Ordering::Acquire);
            let shared_handled = if address != 0 {
                // SAFETY: registration publishes only an immutable function pointer.
                let dispatcher =
                    unsafe { core::mem::transmute::<usize, fn(usize) -> bool>(address) };
                dispatcher(vector)
            } else {
                false
            };
            // Do not short-circuit: direct handlers consume the ISR state latched
            // by the shared pass. All sources must be acknowledged before EOI.
            let direct_handled = IRQ_HANDLER_TABLE.handle(vector);
            if !shared_handled && !direct_handled {
                warn!("Unhandled IRQ {vector}");
            }
            unsafe { super::local_apic().end_of_interrupt() };
            Some(vector)
        }

        /// Sends an inter-processor interrupt (IPI) to the specified target CPU or all CPUs.
        fn send_ipi(irq_num: usize, target: IpiTarget) {
            match target {
                IpiTarget::Current { cpu_id: _ } => {
                    unsafe {
                        super::local_apic().send_ipi_self(irq_num as _);
                    };
                }
                IpiTarget::Other { cpu_id } => {
                    let apic_id = crate::cpu::apic_id_for_logical(cpu_id).unwrap_or_else(|| {
                        panic!("logical CPU {cpu_id} has no APIC identity in the MADT topology")
                    });
                    let apic_destination = super::raw_apic_id(apic_id).unwrap_or_else(|| {
                        panic!("logical CPU {cpu_id} APIC ID {apic_id:#x} cannot be addressed")
                    });
                    unsafe {
                        super::local_apic().send_ipi(irq_num as _, apic_destination);
                    };
                }
                IpiTarget::AllExceptCurrent {
                    cpu_id: _,
                    cpu_num: _,
                } => {
                    use x2apic::lapic::IpiAllShorthand;
                    unsafe {
                        super::local_apic()
                            .send_ipi_all(irq_num as _, IpiAllShorthand::AllExcludingSelf);
                    };
                }
            }
        }
    }
}
