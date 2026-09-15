//! Pure `mmap(2)` admission policy.
//!
//! Mirrors the decision Linux v7.2.3 `mm/mmap.c:do_mmap()` takes from the raw
//! `flags` word: which extended bits `MAP_SHARED_VALIDATE` accepts.  The kernel
//! owns the address space, the descriptor, and the VMA topology; this module owns
//! only the numbers Linux compares, so every rule here is host-testable.

use crate::MmError;

/// Bytes per page on x86-64, the unit every `rlimit(RLIMIT_*)` conversion uses.
pub const PAGE_SIZE: u64 = 4096;

/// `include/linux/mman.h:LEGACY_MAP_MASK` as x86-64 spells it.
///
/// ```c
/// #define LEGACY_MAP_MASK (MAP_SHARED \
/// 		| MAP_PRIVATE \
/// 		| MAP_FIXED \
/// 		| MAP_ANONYMOUS \
/// 		| MAP_DENYWRITE \
/// 		| MAP_EXECUTABLE \
/// 		| MAP_UNINITIALIZED \
/// 		| MAP_GROWSDOWN \
/// 		| MAP_LOCKED \
/// 		| MAP_NORESERVE \
/// 		| MAP_POPULATE \
/// 		| MAP_NONBLOCK \
/// 		| MAP_STACK \
/// 		| MAP_HUGETLB \
/// 		| MAP_32BIT \
/// 		| MAP_ABOVE4G \
/// 		| MAP_HUGE_2MB \
/// 		| MAP_HUGE_1GB)
/// ```
///
/// `MAP_ABOVE4G` is only nonzero on x86 (`arch/x86/include/uapi/asm/mman.h:6`
/// defines it as `0x80`).  `MAP_EXECUTABLE` and `MAP_DENYWRITE` are named there
/// even though the kernel ignores both.
pub const LEGACY_MAP_MASK: u32 = MAP_SHARED
    | MAP_PRIVATE
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_DENYWRITE
    | MAP_EXECUTABLE
    | MAP_UNINITIALIZED
    | MAP_GROWSDOWN
    | MAP_LOCKED
    | MAP_NORESERVE
    | MAP_POPULATE
    | MAP_NONBLOCK
    | MAP_STACK
    | MAP_HUGETLB
    | MAP_32BIT
    | MAP_ABOVE4G
    | MAP_HUGE_2MB
    | MAP_HUGE_1GB;

const MAP_SHARED: u32 = 0x01;
const MAP_PRIVATE: u32 = 0x02;
const MAP_FIXED: u32 = 0x10;
const MAP_ANONYMOUS: u32 = 0x20;
const MAP_32BIT: u32 = 0x40;
const MAP_ABOVE4G: u32 = 0x80;
const MAP_GROWSDOWN: u32 = 0x0100;
const MAP_DENYWRITE: u32 = 0x0800;
const MAP_EXECUTABLE: u32 = 0x1000;
const MAP_LOCKED: u32 = 0x2000;
const MAP_NORESERVE: u32 = 0x4000;
const MAP_POPULATE: u32 = 0x8000;
const MAP_NONBLOCK: u32 = 0x1_0000;
const MAP_STACK: u32 = 0x2_0000;
const MAP_HUGETLB: u32 = 0x4_0000;
const MAP_UNINITIALIZED: u32 = 0x400_0000;
const MAP_HUGE_2MB: u32 = 21 << 26;
const MAP_HUGE_1GB: u32 = 30 << 26;

/// `include/uapi/linux/mman.h:MAP_SYNC`, accepted by `MAP_SHARED_VALIDATE`
/// only for a file whose `file_operations` advertise `FOP_MMAP_SYNC`.
pub const MAP_SYNC: u32 = 0x8_0000;

const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

/// `mm/mmap.c:do_mmap()`'s answer for a `MAP_SHARED_VALIDATE` request.
///
/// The file branch validates the whole flag word against a mask that starts as
/// `LEGACY_MAP_MASK`:
///
/// ```c
/// 		flags_mask = LEGACY_MAP_MASK;
/// 		if (file->f_op->fop_flags & FOP_MMAP_SYNC)
/// 			flags_mask |= MAP_SYNC;
/// 		switch (flags & MAP_TYPE) {
/// 		case MAP_SHARED:
/// 			flags &= LEGACY_MAP_MASK;
/// 			fallthrough;
/// 		case MAP_SHARED_VALIDATE:
/// 			if (flags & ~flags_mask)
/// 				return -EOPNOTSUPP;
/// ```
///
/// `MAP_FIXED_NOREPLACE`, `MAP_SYNC` (without `FOP_MMAP_SYNC`) and
/// `MAP_DROPPABLE` are the bits outside that mask, so they — and only they —
/// are `-EOPNOTSUPP`.
///
/// The branch taken when no file is behind the mapping has no
/// `MAP_SHARED_VALIDATE` case at all:
///
/// ```c
/// 	} else {
/// 		switch (flags & MAP_TYPE) {
/// 		case MAP_SHARED:
/// 			...
/// 		case MAP_DROPPABLE:
/// 			...
/// 		case MAP_PRIVATE:
/// 			...
/// 		default:
/// 			return -EINVAL;
/// 		}
/// 	}
/// ```
///
/// so an anonymous `MAP_SHARED_VALIDATE` is `-EINVAL` whatever the rest of the
/// word says, because the `MAP_TYPE` dispatch happens before any flag test.
/// `is_anonymous` is that dispatch's input, not a hint.
pub const fn map_shared_validate_errno(
    is_anonymous: bool,
    flags: u32,
    file_accepts_map_sync: bool,
) -> Option<i32> {
    if is_anonymous {
        return Some(EINVAL);
    }
    let mask = if file_accepts_map_sync {
        LEGACY_MAP_MASK | MAP_SYNC
    } else {
        LEGACY_MAP_MASK
    };
    if flags & !mask != 0 {
        Some(EOPNOTSUPP)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_mask_matches_the_x86_64_headers() {
        // Every bit `include/linux/mman.h:LEGACY_MAP_MASK` names, spelled with
        // the values `include/uapi/linux/mman.h`,
        // `include/uapi/asm-generic/mman-common.h` and
        // `arch/x86/include/uapi/asm/mman.h` give them.
        for bit in [
            0x01_u32,        // MAP_SHARED
            0x02,            // MAP_PRIVATE
            0x10,            // MAP_FIXED
            0x20,            // MAP_ANONYMOUS
            0x0800,          // MAP_DENYWRITE
            0x1000,          // MAP_EXECUTABLE
            0x400_0000,      // MAP_UNINITIALIZED
            0x0100,          // MAP_GROWSDOWN
            0x2000,          // MAP_LOCKED
            0x4000,          // MAP_NORESERVE
            0x8000,          // MAP_POPULATE
            0x1_0000,        // MAP_NONBLOCK
            0x2_0000,        // MAP_STACK
            0x4_0000,        // MAP_HUGETLB
            0x40,            // MAP_32BIT
            0x80,            // MAP_ABOVE4G
            21 << 26,        // MAP_HUGE_2MB
            30 << 26,        // MAP_HUGE_1GB
        ] {
            assert_eq!(LEGACY_MAP_MASK & bit, bit, "{bit:#x}");
        }
        // The three extended bits that are deliberately *not* legacy.
        for bit in [0x8_0000_u32 /* MAP_SYNC */, 0x10_0000 /* MAP_FIXED_NOREPLACE */, 0x08 /* MAP_DROPPABLE */] {
            assert_eq!(LEGACY_MAP_MASK & bit, 0, "{bit:#x}");
        }
    }

    #[test]
    fn anonymous_shared_validate_is_einval_whatever_the_other_bits_are() {
        // The no-file branch's `switch (flags & MAP_TYPE)` reaches `default:`
        // for `MAP_SHARED_VALIDATE`, and that dispatch precedes every flag
        // test, so even a fully legacy word is `-EINVAL`.
        assert_eq!(map_shared_validate_errno(true, 0x03, false), Some(22));
        assert_eq!(
            map_shared_validate_errno(true, 0x03 | LEGACY_MAP_MASK, false),
            Some(22)
        );
        // `MAP_FIXED_NOREPLACE` cannot turn it into `-EOPNOTSUPP` either.
        assert_eq!(
            map_shared_validate_errno(true, 0x03 | 0x10_0000, false),
            Some(22)
        );
    }

    #[test]
    fn file_shared_validate_rejects_exactly_the_non_legacy_bits() {
        // A legacy word is accepted, including the bits this kernel does not
        // model (`MAP_EXECUTABLE`, `MAP_UNINITIALIZED`, `MAP_ABOVE4G`).
        assert_eq!(map_shared_validate_errno(false, 0x03, false), None);
        assert_eq!(
            map_shared_validate_errno(false, 0x03 | LEGACY_MAP_MASK, false),
            None
        );
        // `MAP_FIXED_NOREPLACE` and `MAP_DROPPABLE` are the observable ones.
        assert_eq!(
            map_shared_validate_errno(false, 0x03 | 0x10_0000, false),
            Some(95)
        );
        assert_eq!(map_shared_validate_errno(false, 0x03 | 0x08, false), Some(95));
        // `MAP_SYNC` needs `FOP_MMAP_SYNC` on the file behind the mapping.
        assert_eq!(
            map_shared_validate_errno(false, 0x03 | MAP_SYNC, false),
            Some(95)
        );
        assert_eq!(
            map_shared_validate_errno(false, 0x03 | MAP_SYNC, true),
            None
        );
        // A `MAP_SHARED_VALIDATE` word's `MAP_TYPE` field is part of the word,
        // so the request itself must not be reported as a stray bit.
        assert_eq!(map_shared_validate_errno(false, 0x03, false), None);
    }
}
