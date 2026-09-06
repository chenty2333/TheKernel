//! Dummy implementation of platform-related interfaces defined in [`axplat`].

use core::cell::UnsafeCell;

#[cfg(feature = "irq")]
use axplat::irq::{IpiTarget, IrqHandler, IrqIf};
use axplat::{
    console::ConsoleIf,
    impl_plat_interface,
    init::InitIf,
    mem::{MemIf, RawRange},
    power::PowerIf,
    time::TimeIf,
};
use spin::Lazy;

struct DummyInit;
struct DummyConsole;
struct DummyMem;
struct DummyTime;
struct DummyPower;
#[cfg(feature = "irq")]
struct DummyIrq;

const HOST_PAGE_ARENA_SIZE: usize = 32 * 1024 * 1024;

/// Host-test backing memory for the dummy platform's page allocator.
///
/// The dummy platform never represents real physical memory.  Its page-table
/// implementation nevertheless needs a bounded region whose physical and
/// virtual names round-trip while host tests manipulate mappings.
#[repr(align(4096))]
struct HostPageArena(UnsafeCell<[u8; HOST_PAGE_ARENA_SIZE]>);

// The host page allocator has exclusive ownership of this region after the
// kernel test bootstrap initializes it.  `UnsafeCell` keeps the storage in
// writable BSS without creating aliases through Rust references.
unsafe impl Sync for HostPageArena {}

static HOST_PAGE_ARENA: HostPageArena = HostPageArena(UnsafeCell::new([0; HOST_PAGE_ARENA_SIZE]));
static HOST_PAGE_ARENA_RANGE: Lazy<[RawRange; 1]> = Lazy::new(|| {
    [(
        HOST_PAGE_ARENA.0.get() as *mut u8 as usize,
        HOST_PAGE_ARENA_SIZE,
    )]
});

fn host_page_arena_range() -> RawRange {
    HOST_PAGE_ARENA_RANGE[0]
}

fn host_page_arena_address(address: usize) -> usize {
    let (start, size) = host_page_arena_range();
    assert!(
        (start..start + size).contains(&address),
        "dummy host physical address outside page arena: {address:#x}"
    );
    address
}

#[impl_plat_interface]
impl InitIf for DummyInit {
    fn init_early(_cpu_id: usize, _arg: usize) {}

    #[cfg(feature = "smp")]
    fn init_early_secondary(_cpu_id: usize) {}

    fn init_later(_cpu_id: usize, _arg: usize) {}

    #[cfg(feature = "smp")]
    fn init_later_secondary(_cpu_id: usize) {}
}

#[impl_plat_interface]
impl ConsoleIf for DummyConsole {
    fn write_bytes(_bytes: &[u8]) {
        unimplemented!()
    }

    fn read_bytes(_bytes: &mut [u8]) -> usize {
        unimplemented!()
    }

    #[cfg(feature = "irq")]
    fn irq_num() -> Option<usize> {
        None
    }
}

#[impl_plat_interface]
impl MemIf for DummyMem {
    fn phys_ram_ranges() -> &'static [RawRange] {
        &HOST_PAGE_ARENA_RANGE[..]
    }

    fn reserved_phys_ram_ranges() -> &'static [RawRange] {
        &[]
    }

    fn mmio_ranges() -> &'static [RawRange] {
        &[]
    }

    fn phys_to_virt(paddr: memory_addr::PhysAddr) -> memory_addr::VirtAddr {
        va!(host_page_arena_address(paddr.as_usize()))
    }

    fn virt_to_phys(vaddr: memory_addr::VirtAddr) -> memory_addr::PhysAddr {
        pa!(host_page_arena_address(vaddr.as_usize()))
    }

    fn kernel_aspace() -> (memory_addr::VirtAddr, usize) {
        (va!(0), 0)
    }
}

#[impl_plat_interface]
impl TimeIf for DummyTime {
    fn current_ticks() -> u64 {
        0
    }

    fn ticks_to_nanos(ticks: u64) -> u64 {
        ticks
    }

    fn nanos_to_ticks(nanos: u64) -> u64 {
        nanos
    }

    fn epochoffset_nanos() -> u64 {
        0
    }

    #[cfg(feature = "irq")]
    fn irq_num() -> usize {
        0
    }

    #[cfg(feature = "irq")]
    fn set_oneshot_timer(_deadline_ns: u64) {}
}

#[impl_plat_interface]
impl PowerIf for DummyPower {
    #[cfg(feature = "smp")]
    fn cpu_boot(_cpu_id: usize, _stack_top_paddr: usize) {}

    fn system_off() -> ! {
        unimplemented!()
    }

    fn cpu_num() -> usize {
        1
    }
}

#[cfg(feature = "irq")]
#[impl_plat_interface]
impl IrqIf for DummyIrq {
    fn set_enable(_irq: usize, _enabled: bool) {}

    fn register(_irq: usize, _handler: IrqHandler) -> bool {
        false
    }

    fn unregister(_irq: usize) -> Option<IrqHandler> {
        None
    }

    fn handle(_irq: usize) -> Option<usize> {
        None
    }

    fn send_ipi(_irq: usize, _target: IpiTarget) {}
}
