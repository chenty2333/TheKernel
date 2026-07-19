//! User address space management and user-space memory access.

mod access;
mod aspace;
mod fault;
mod io;
mod loader;
mod remap;
mod stats;
mod tlb;
mod userfaultfd;

pub use self::{access::*, aspace::*, io::*, loader::*, stats::*};
pub(crate) use self::{
    fault::handle_user_page_fault,
    remap::remap_user_mapping,
    tlb::{
        init as init_tlb_shootdown, repair_local_spurious_fault, retire_after_tlb_grace,
        synchronize_icache, synchronize_tlb, synchronize_tlb_and_icache,
    },
    userfaultfd::*,
};

pub(crate) fn checked_align_up(value: usize, align: usize) -> Option<usize> {
    if !align.is_power_of_two() {
        return None;
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

pub(crate) fn checked_align_up_4k(value: usize) -> Option<usize> {
    checked_align_up(value, memory_addr::PAGE_SIZE_4K)
}
