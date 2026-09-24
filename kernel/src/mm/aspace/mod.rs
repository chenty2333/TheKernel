use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    fmt,
    ops::DerefMut,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering, fence},
};

use axerrno::{AxError, AxResult, ax_bail};
use axhal::{
    mem::phys_to_virt,
    paging::{
        MappingFlags, PageSize, PageTable, PagingError, PagingHandlerImpl, Pkey,
        PrepareTableFramesError, PreparedPageTableFrames,
    },
    trap::PageFaultFlags,
};
use axsync::Mutex;
use hashbrown::{HashMap, hash_map::Entry};
use kernel_guard::NoPreemptIrqSave;
use kspin::SpinNoIrq;
use linux_raw_sys::general::{RLIM_INFINITY, RLIMIT_STACK};
use memory_addr::{
    MemoryAddr, PAGE_SIZE_4K, PageIter4K, PhysAddr, VirtAddr, VirtAddrRange, is_aligned_4k,
};
use memory_set::{MappingLineage, MappingResult, MemoryArea, MemorySet};
use page_table_multiarch::{ReplacedPteRun, x86_64::X64PTE};
use tk_linux_mm::{
    AddressSpaceId, ExpectedMapping, FaultDisposition, FaultHandlerId, InvalidationRange,
    InvalidationReason, MappingAccess, MappingGeneration, MappingId, MappingKind, MappingSnapshot,
    MmError, PageRange, PinBudget, PinBudgetCharge, PinOwner, PinQuota, PinRegistry, PinRequest,
    PinReservation, PinToken, UffdRegisterMode, UffdRegistration,
};

use super::{
    DeferredMappingFinalizer, DeferredUffdWake, LockExternalUffdOutcome, OptionalUffdPlan,
    PreparedRemapUffd, PreparedUffdMutation, RemapUffdOutcome, UffdFaultLeafState,
    UffdIcacheSynchronization, UffdPagePublication, UffdRemapKind, UffdResolverLease,
    asid::{AddressSpaceToken, HardwareAddressSpaceId, reserve_hardware_address_space_id},
    checked_align_up_4k,
    ldt::{ENTRIES, Ldt, UserDesc},
};
use crate::task::{AsThread, has_pending_sigkill};

mod alias_registry;
mod backend;
mod mapping;

pub use self::backend::*;
pub(crate) use self::{
    alias_registry::{
        AliasLease, PendingAliasLease, SharedBackingKey, reserve_alias_mutation,
        wait_for_alias_publication,
    },
    mapping::{
        FileLikeMappingLease, FileMappingLease, FileMappingSharing, MadviseReadahead, MadviseThp,
    },
};

mod addr_space;
mod cet;
mod clone;
mod fault;
mod fault_types;
mod identity;
mod map;
mod policy;
mod populate;
mod protect_types;
mod query;
mod setup;
mod thp;
mod tlb;
mod transaction;

pub use self::{addr_space::*, fault_types::*};
pub(crate) use self::{identity::*, protect_types::*, tlb::*};

#[cfg(test)]
mod tests;
