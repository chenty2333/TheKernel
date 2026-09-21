//! User address space management and user-space memory access.

mod access;
mod asid;
mod aspace;
mod fault;
mod io;
pub(crate) mod ldt;
mod loader;
mod mapping_finalizer;
mod pressure;
mod remap;
pub(crate) mod secret;
mod stats;
mod swap;
mod thp;
mod tlb;
mod usercopy;
mod userfaultfd;

pub use self::{
    access::*, asid::AddressSpaceToken, aspace::*, io::*, loader::*, pressure::*, stats::*, swap::*,
};
pub(crate) use self::{
    asid::init as init_hardware_asids,
    fault::handle_user_page_fault,
    mapping_finalizer::{
        DeferredMappingFinalizer, MappingFinalizer, drain_deferred_mapping_finalizers,
        has_deferred_mapping_finalizer_work,
    },
    pressure::init_memory_pressure,
    remap::remap_user_mapping,
    thp::init_khugepaged,
    tlb::{
        init as init_tlb_shootdown, repair_local_spurious_fault, retire_after_tlb_grace,
        synchronize_icache, synchronize_kernel_text_patch, synchronize_tlb,
        synchronize_tlb_and_icache, synchronize_tlb_for_addr_space,
    },
    usercopy::{
        AddressSpaceUserMemory, UserMemoryCapability, copy_struct_from_user, copy_struct_to_user,
        map_usercopy_error, with_user_memory,
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
