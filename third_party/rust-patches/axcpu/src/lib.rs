#![cfg_attr(not(test), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

#[macro_use]
extern crate log;

#[macro_use]
extern crate memory_addr;

#[cfg(feature = "asid-fast-switch")]
#[inline]
const fn can_retain_user_tlb(
    current_root: usize,
    current_asid: usize,
    current_generation: u64,
    next_root: usize,
    next_asid: usize,
    next_generation: u64,
) -> bool {
    // This predicate relies on the caller never recycling a nonzero numeric
    // ASID during the boot.  TheKernel's bounded allocator permanently falls
    // back to ASID 0 on exhaustion because its current global TLB grace is not
    // a quiescence protocol.
    current_asid != 0
        && next_asid != 0
        && current_generation != 0
        && current_generation == next_generation
        && (current_asid != next_asid || current_root == next_root)
}

#[macro_use]
pub mod trap;

#[cfg(feature = "uspace")]
mod uspace_common;

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use self::x86_64::*;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        mod riscv;
        pub use self::riscv::*;
    } else if #[cfg(target_arch = "aarch64")]{
        mod aarch64;
        pub use self::aarch64::*;
    } else if #[cfg(any(target_arch = "loongarch64"))] {
        mod loongarch64;
        pub use self::loongarch64::*;
    }
}

#[cfg(all(test, feature = "asid-fast-switch"))]
mod tests {
    use super::can_retain_user_tlb;

    #[test]
    fn distinct_nonzero_ids_in_one_non_reused_generation_retain_tlb() {
        assert!(can_retain_user_tlb(0x1000, 1, 1, 0x2000, 2, 1));
    }

    #[test]
    fn legacy_or_cross_generation_transitions_require_full_flush() {
        assert!(!can_retain_user_tlb(0x1000, 0, 0, 0x2000, 1, 1));
        assert!(!can_retain_user_tlb(0x1000, 1, 1, 0x2000, 0, 0));
        assert!(!can_retain_user_tlb(0x1000, 1, 1, 0x2000, 2, 2));
    }

    #[test]
    fn same_numeric_id_with_a_different_root_requires_full_flush() {
        assert!(!can_retain_user_tlb(0x1000, 1, 1, 0x2000, 1, 1));
        assert!(can_retain_user_tlb(0x1000, 1, 1, 0x1000, 1, 1));
    }
}
