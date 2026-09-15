//! Pure validation order for the NUMA memory-policy ABI.
//!
//! Linux resolves `set_mempolicy(2)`, `mbind(2)` and `get_mempolicy(2)` in a
//! fixed order, and that order is the ABI: which of two malformed arguments is
//! reported decides which errno userspace observes. This module owns the
//! argument-only part of that order — the mode/flag table, the node-mask
//! bounds, and `mbind`'s address alignment — so it can be tested on the host
//! without an address space or a task.
//!
//! Matching Linux v7.2.3 `mm/mempolicy.c`: `sanitize_mpol_flags()`,
//! `get_nodes()`, `mpol_new()`, `mpol_set_nodemask()`, `kernel_mbind()`,
//! `do_mbind()`, `kernel_set_mempolicy()`, `do_set_mempolicy()`,
//! `kernel_get_mempolicy()`, `do_get_mempolicy()`, `copy_nodes_to_user()`.
//!
//! TheKernel is a single-node kernel, so the kernel's `node_states[N_MEMORY]`
//! intersection is `{0}`. Every admission rule below is Linux's; only the size
//! of the allowed-node set differs, and it keeps the policies' Linux meaning
//! degenerate rather than changing it.

/// `MPOL_DEFAULT`: use the task's (or VMA's) inherited policy.
pub const MPOL_DEFAULT: u32 = 0;
/// `MPOL_PREFERRED`: prefer one node, fall back anywhere.
pub const MPOL_PREFERRED: u32 = 1;
/// `MPOL_BIND`: allocate only from the given nodes.
pub const MPOL_BIND: u32 = 2;
/// `MPOL_INTERLEAVE`: round-robin across the given nodes.
pub const MPOL_INTERLEAVE: u32 = 3;
/// `MPOL_LOCAL`: allocate on the local node.
pub const MPOL_LOCAL: u32 = 4;
/// `MPOL_PREFERRED_MANY`: prefer the given nodes, fall back anywhere.
pub const MPOL_PREFERRED_MANY: u32 = 5;
/// `MPOL_WEIGHTED_INTERLEAVE`: interleave by per-node bandwidth weights.
pub const MPOL_WEIGHTED_INTERLEAVE: u32 = 6;
/// Exclusive upper bound of the policy-mode enum (`MPOL_MAX`).
pub const MPOL_MAX: u32 = 7;

/// `MPOL_F_STATIC_NODES`: `nmask` names nodes literally.
pub const MPOL_F_STATIC_NODES: u32 = 1 << 15;
/// `MPOL_F_RELATIVE_NODES`: `nmask` names nodes relative to the allowed set.
pub const MPOL_F_RELATIVE_NODES: u32 = 1 << 14;
/// `MPOL_F_NUMA_BALANCING`: enable NUMA-balancing migration for the policy.
pub const MPOL_F_NUMA_BALANCING: u32 = 1 << 13;
/// `MPOL_MODE_FLAGS`: every mode flag an `int` mode argument may carry.
pub const MPOL_MODE_FLAGS: u32 =
    MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES | MPOL_F_NUMA_BALANCING;
/// `MPOL_USER_NODEMASK_FLAGS`: the flags for which Linux keeps the caller's
/// own mask in `pol->w.user_nodemask` and reports *that* from
/// `get_mempolicy(2)` instead of the intersected `pol->nodes`.
pub const MPOL_USER_NODEMASK_FLAGS: u32 = MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES;

/// `MPOL_F_NODE` for `get_mempolicy(2)`: return a node id, not a policy.
pub const MPOL_F_NODE: usize = 1 << 0;
/// `MPOL_F_ADDR` for `get_mempolicy(2)`: look the policy up by address.
pub const MPOL_F_ADDR: usize = 1 << 1;
/// `MPOL_F_MEMS_ALLOWED` for `get_mempolicy(2)`: return the allowed set.
pub const MPOL_F_MEMS_ALLOWED: usize = 1 << 2;
/// `MPOL_MF_STRICT` for `mbind(2)`: fail on pages that cannot conform.
pub const MPOL_MF_STRICT: usize = 1 << 0;
/// `MPOL_MF_MOVE` for `mbind(2)`: migrate the task's own pages.
pub const MPOL_MF_MOVE: usize = 1 << 1;
/// `MPOL_MF_MOVE_ALL` for `mbind(2)`: migrate every page (needs `CAP_SYS_NICE`).
pub const MPOL_MF_MOVE_ALL: usize = 1 << 2;
/// `MPOL_MF_VALID`: the only `mbind(2)` flags Linux accepts.
pub const MPOL_MF_VALID: usize = MPOL_MF_STRICT | MPOL_MF_MOVE | MPOL_MF_MOVE_ALL;

/// The page size `get_nodes()`/`do_mbind()` measure against; TheKernel is 4K-only.
pub const PAGE_SIZE: usize = 4096;
/// `PAGE_SIZE * BITS_PER_BYTE`; `get_nodes()` rejects a larger `maxnode`.
pub const MAX_NODEMASK_BITS: usize = PAGE_SIZE * 8;

/// What the caller asked the kernel to install, after mode/flag splitting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MempolicyRequest {
    /// The policy mode with every `MPOL_MODE_FLAGS` bit removed.
    pub mode: u32,
    /// The mode flags the caller supplied.
    pub mode_flags: u32,
    /// The `nmask` contents narrowed to the allowed node set.
    pub nodes: usize,
    /// The parsed `nmask` before it was narrowed to the allowed node set.
    pub user_nodes: usize,
    /// Whether the caller supplied any node mask at all.
    pub has_nodes: bool,
}

/// Which argument a rejected mempolicy request was rejected for.
///
/// The variant is what makes the errno order testable: `mode` is always
/// examined before `nodes`, matching `sanitize_mpol_flags()` running before
/// `get_nodes()` in `kernel_mbind()`/`kernel_set_mempolicy()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MempolicyError {
    /// The mode is not a `MPOL_*` policy, or the flags contradict it. `EINVAL`.
    InvalidMode,
    /// `maxnode` exceeds `PAGE_SIZE * BITS_PER_BYTE`. `EINVAL`.
    NodeMaskTooLong,
    /// A node the kernel cannot represent was requested. `EINVAL`.
    NodeOutOfRange,
    /// The mode needs a non-empty mask and none of the requested nodes is
    /// allowed, or the mode forbids a mask outright. `EINVAL`.
    EmptyNodeMask,
    /// `MPOL_MF_MOVE_ALL` was requested without `CAP_SYS_NICE`. `EPERM`.
    MoveAllNotPermitted,
    /// `start` is not page-aligned. `EINVAL`.
    UnalignedStart,
    /// The page-aligned range wraps past the end of the address space. `EINVAL`.
    RangeOverflow,
}

/// `get_nodes()` in Linux v7.2.3 `mm/mempolicy.c`.
///
/// `maxnode` is a count of bits, so a mask naming *n* nodes passes *n + 1*.
/// Linux decrements first, so `maxnode` of 0 or 1 is the "no mask" spelling that
/// every mode accepts even when `nmask` is non-NULL.
///
/// Otherwise the bits from `maxnode - 1` up to capacity are scanned word by
/// word, *from the end*. Up to `MAX_NUMNODES` they are kept; above it Linux
/// only verifies that they are zero, and a set bit there is `EINVAL` — the
/// kernel cannot name a node it does not have. Bits at or above `maxnode` are
/// read and must also be zero. Because the scan starts at the top, a mask that
/// sets only a low bit is admitted for any `maxnode`, however large.
///
/// Returns the accepted mask together with whether `nmask` was supplied,
/// because `mpol_new()` distinguishes "no nodes" from "node 0".
pub fn parse_node_mask(
    maxnode: usize,
    words: &[usize],
    nmask_supplied: bool,
) -> Result<(usize, bool), MempolicyError> {
    // Linux: `--maxnode; if (maxnode == 0 || !nmask) return 0;`
    let Some(bits) = maxnode.checked_sub(1) else {
        return Ok((0, false));
    };
    if bits == 0 || !nmask_supplied {
        return Ok((0, false));
    }
    // Linux only rejects the size when a mask was actually supplied.
    if bits > MAX_NODEMASK_BITS {
        return Err(MempolicyError::NodeMaskTooLong);
    }

    // Linux scans from the highest word down. Everything at or above
    // `MAX_NUMNODES` is admitted only when zero, because `nodemask_t` cannot
    // name such a node. At and below it, the bits below `maxnode` are the ones
    // `get_bitmap()` copies out; the first word is the only one that can hold
    // them, since `MAX_NUMNODES == usize::BITS` in this configuration.
    let mut mask = 0usize;
    for (index, word) in words.iter().copied().enumerate() {
        let base = index * usize::BITS as usize;
        if base >= bits {
            break;
        }
        let keep = (bits - base).min(usize::BITS as usize);
        let masked = (word << (usize::BITS as usize - keep)) >> (usize::BITS as usize - keep);
        let above_capacity = if base >= MAX_NUMNODES {
            masked
        } else {
            masked & !mask_below((MAX_NUMNODES - base).min(keep))
        };
        if above_capacity != 0 {
            return Err(MempolicyError::NodeOutOfRange);
        }
        if base < usize::BITS as usize {
            mask |= (masked & mask_below(keep)) << base;
        }
    }
    Ok((mask, true))
}

/// `(1 << bits) - 1`, with `bits == 64` saturated to all ones.
const fn mask_below(bits: usize) -> usize {
    if bits >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << bits) - 1
    }
}

/// How many `usize` words of the user mask `get_nodes()` reads for `maxnode`.
///
/// Linux walks the window from the top and stops once it reaches
/// `MAX_NUMNODES`, so this is exactly the number of words a caller must be able
/// to read; a shorter buffer is `EFAULT` in the kernel because the read fails.
/// It is at most `MAX_NODEMASK_BITS / usize::BITS + 1`, i.e. 513 for x86_64, so
/// a caller can bound its copy without trusting `maxnode`.
pub const fn scanned_words(maxnode: usize) -> usize {
    let Some(bits) = maxnode.checked_sub(1) else {
        return 0;
    };
    if bits == 0 {
        return 0;
    }
    if bits <= MAX_NUMNODES {
        return 1;
    }
    // Every word below `bits` is read until the scan can clamp.
    bits.div_ceil(usize::BITS as usize)
}

/// Linux `MAX_NUMNODES` for this configuration; the kernel keeps at most one
/// word of parsed mask, and this is the size of that word in bits.
pub const MAX_NUMNODES: usize = 64;
/// Linux `nr_node_ids`: node 0 is the only memory node TheKernel has.
pub const NR_NODE_IDS: usize = 1;
/// The set of nodes a memory policy may name: `{0}`.
pub const ALLOWED_NODEMASK: usize = 0b1;

/// `sanitize_mpol_flags()` in Linux v7.2.3 `mm/mempolicy.c`.
///
/// This is the first step of both `kernel_mbind()` and
/// `kernel_set_mempolicy()`, and it runs before the node mask is read. It
/// splits the `MPOL_MODE_FLAGS` bits out of the mode, rejects a mode at or
/// above `MPOL_MAX`, rejects the mutually exclusive
/// `MPOL_F_STATIC_NODES|MPOL_F_RELATIVE_NODES` combination, and rejects
/// `MPOL_F_NUMA_BALANCING` for every mode except `MPOL_BIND` and
/// `MPOL_PREFERRED_MANY`.
///
/// Returns the mode and the mode flags, so a caller that needs only the mode
/// error can report it before touching user memory.
pub const fn sanitize_mode_flags(mode_with_flags: u32) -> Result<(u32, u32), MempolicyError> {
    let mode_flags = mode_with_flags & MPOL_MODE_FLAGS;
    let mode = mode_with_flags & !MPOL_MODE_FLAGS;
    if mode >= MPOL_MAX {
        return Err(MempolicyError::InvalidMode);
    }
    if mode_flags & MPOL_F_STATIC_NODES != 0 && mode_flags & MPOL_F_RELATIVE_NODES != 0 {
        return Err(MempolicyError::InvalidMode);
    }
    if mode_flags & MPOL_F_NUMA_BALANCING != 0 && mode != MPOL_BIND && mode != MPOL_PREFERRED_MANY {
        return Err(MempolicyError::InvalidMode);
    }
    Ok((mode, mode_flags))
}

/// `sanitize_mpol_flags()` followed by `mpol_new()` and `mpol_set_nodemask()`.
///
/// `requested` is the `nmask` contents (or 0 when `nmask` was NULL) and
/// `has_nodes` records whether the caller supplied a mask at all; `allowed` is
/// the intersection of the caller's cpuset-allowed set with
/// `node_states[N_MEMORY]`.
///
/// The order is Linux's and is observable:
/// 1. `sanitize_mpol_flags()` rejects an out-of-range mode and a
///    `STATIC|RELATIVE` combination before `nmask` is ever read.
/// 2. `MPOL_F_NUMA_BALANCING` is only meaningful for `MPOL_BIND` and
///    `MPOL_PREFERRED_MANY`, and Linux rejects it for every other mode here.
/// 3. `mpol_new()` decides whether the mode needs a non-empty mask.
/// 4. `mpol_set_nodemask()` intersects with the allowed set and re-checks, so a
///    mask that names no allowed node is `EINVAL` for those modes.
pub fn validate(
    mode_with_flags: u32,
    requested: usize,
    has_nodes: bool,
    allowed: usize,
) -> Result<MempolicyRequest, MempolicyError> {
    // Step 1: `sanitize_mpol_flags()`, before the mask is considered.
    let (mode, mode_flags) = sanitize_mode_flags(mode_with_flags)?;

    // Step 2: `mpol_set_nodemask()` intersects the request with the allowed memory set,
    // except under `MPOL_F_RELATIVE_NODES`: there the *n*-th set bit of the user
    // mask names the *n*-th allowed node, so the result is contained in the
    // allowed set by construction. TheKernel has one allowed node, which makes
    // any non-empty relative mask name it.
    let nodes = if mode_flags & MPOL_F_RELATIVE_NODES != 0 {
        if requested != 0 { allowed } else { 0 }
    } else {
        requested & allowed
    };

    let user_empty = !has_nodes || requested == 0;
    let effective_empty = nodes == 0 || !has_nodes;

    match mode {
        MPOL_DEFAULT => {
            // `mpol_new()`: a non-empty node list for MPOL_DEFAULT is EINVAL.
            if !user_empty {
                return Err(MempolicyError::EmptyNodeMask);
            }
        }
        MPOL_PREFERRED => {
            // `mpol_new()` inspects only the *user* mask when it decides to
            // rewrite an empty `MPOL_PREFERRED` into `MPOL_LOCAL`, and a
            // user-nodemask flag makes that emptiness an explicit error. The
            // rewritten (or surviving) preferred policy is then built by
            // `mpol_new_preferred()` from the mask *intersected* with the
            // allowed set, which rejects an empty intersection -- so a mask
            // naming only nodes the caller may not use is `EINVAL` even
            // without `MPOL_F_STATIC_NODES`.
            if !has_nodes || requested == 0 {
                if mode_flags & (MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES) != 0 {
                    return Err(MempolicyError::EmptyNodeMask);
                }
            } else if nodes == 0 {
                return Err(MempolicyError::EmptyNodeMask);
            }
        }
        MPOL_LOCAL => {
            // `mpol_new()`: MPOL_LOCAL requires an empty list and no
            // user-nodemask flag.
            if !user_empty || mode_flags & (MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES) != 0 {
                return Err(MempolicyError::EmptyNodeMask);
            }
        }
        MPOL_BIND | MPOL_INTERLEAVE | MPOL_PREFERRED_MANY | MPOL_WEIGHTED_INTERLEAVE => {
            // These modes always need a mask, and it must name an allowed node
            // (`mpol_new()` then `mpol_new_nodemask()`).
            if user_empty || effective_empty {
                return Err(MempolicyError::EmptyNodeMask);
            }
        }
        _ => return Err(MempolicyError::InvalidMode),
    }

    Ok(MempolicyRequest {
        mode,
        mode_flags,
        nodes: if has_nodes { nodes } else { 0 },
        user_nodes: requested,
        has_nodes,
    })
}

/// `MPOL_PREFERRED`/`MPOL_LOCAL` normalisation performed by `mpol_new()`.
///
/// Linux rewrites an empty `MPOL_PREFERRED` into `MPOL_LOCAL`, and
/// `mpol_new_preferred()` narrows a preferred list to its first node. Both are
/// observable through `get_mempolicy(2)`, so the kernel stores the result of
/// this function rather than the raw request.
pub const fn effective_policy(request: MempolicyRequest) -> (u32, usize) {
    if request.mode == MPOL_PREFERRED {
        if !request.has_nodes || request.nodes == 0 {
            return (MPOL_LOCAL, 0);
        }
        // `first_node()` is the lowest set bit.
        return (MPOL_PREFERRED, request.nodes & request.nodes.wrapping_neg());
    }
    (request.mode, request.nodes)
}

/// `mpol_store_user_nodemask()`: whether `get_mempolicy(2)` reports the
/// caller's own mask.
///
/// `mpol_set_nodemask()` stores `*nodes` in `pol->w.user_nodemask` for a
/// policy carrying `MPOL_F_STATIC_NODES` or `MPOL_F_RELATIVE_NODES`, and
/// `do_get_mempolicy()` then reports that stored mask rather than the
/// intersected `pol->nodes`.
pub const fn stores_user_nodemask(mode_flags: u32) -> bool {
    mode_flags & MPOL_USER_NODEMASK_FLAGS != 0
}

/// The `*policy` value `do_get_mempolicy()` reports for a stored policy.
///
/// Linux stores `*policy = pol->mode; *policy |= (pol->flags & MPOL_MODE_FLAGS)`,
/// so the shaping flags the caller passed to `set_mempolicy(2)` or `mbind(2)`
/// come back together with the mode instead of being dropped.
pub const fn reported_policy(mode: u32, mode_flags: u32) -> u32 {
    mode | (mode_flags & MPOL_MODE_FLAGS)
}

/// `kernel_mbind()`'s argument order, minus the user memory access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MbindPlan {
    /// Page-aligned start of the affected range.
    pub start: usize,
    /// Page-aligned length; never zero when `Ok` is returned.
    pub len: usize,
    /// Bind flags after Linux's `MPOL_DEFAULT` adjustment.
    pub flags: usize,
}

/// `do_mbind()` in Linux v7.2.3 `mm/mempolicy.c`.
///
/// `kernel_mbind()` runs `sanitize_mpol_flags()` and `get_nodes()` *before*
/// calling this, so an invalid mode or mask is reported even for a zero-length
/// range. This function performs only the checks `do_mbind()` itself makes:
/// 1. Unknown `MPOL_MF_*` bits are `EINVAL` before the address is examined.
/// 2. `MPOL_MF_MOVE_ALL` without `CAP_SYS_NICE` is `EPERM`.
/// 3. An address that is not page-aligned is `EINVAL` — Linux does *not* round
///    it down; `do_mbind()` masks it only after this check.
/// 4. `PAGE_ALIGN(len)` overflow past the address space is `EINVAL`.
/// 5. An empty page-aligned range is a successful no-op.
pub fn plan_mbind(
    start: usize,
    len: usize,
    mode: u32,
    flags: usize,
    has_move_all_capability: bool,
) -> Result<Option<MbindPlan>, MempolicyError> {
    if flags & !MPOL_MF_VALID != 0 {
        return Err(MempolicyError::InvalidMode);
    }
    if flags & MPOL_MF_MOVE_ALL != 0 && !has_move_all_capability {
        return Err(MempolicyError::MoveAllNotPermitted);
    }
    if start & (PAGE_SIZE - 1) != 0 {
        return Err(MempolicyError::UnalignedStart);
    }

    // `len = PAGE_ALIGN(len)` uses the unchecked macro, so a near-`usize::MAX`
    // length wraps to a small aligned value first.
    let aligned_len = len.wrapping_add(PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let end = start.wrapping_add(aligned_len);
    if end < start {
        return Err(MempolicyError::RangeOverflow);
    }
    if end == start {
        return Ok(None);
    }

    // `MPOL_DEFAULT` cannot migrate pages to conform, so Linux drops STRICT.
    let flags = if mode == MPOL_DEFAULT {
        flags & !MPOL_MF_STRICT
    } else {
        flags
    };

    Ok(Some(MbindPlan {
        start,
        len: aligned_len,
        flags,
    }))
}

/// Why `get_mempolicy(2)` rejected its arguments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GetMempolicyError {
    /// The flag word, or a flag combination, is not one Linux defines.
    InvalidFlags,
    /// `nodemask` was supplied with a `maxnode` below `nr_node_ids`.
    NodeMaskTooShort,
    /// `addr` was non-zero without `MPOL_F_ADDR`.
    UnexpectedAddress,
}

/// `kernel_get_mempolicy()` then `do_get_mempolicy()` argument validation.
///
/// Order, and therefore errno precedence, is Linux's:
/// 1. `kernel_get_mempolicy()` rejects a supplied `nodemask` whose `maxnode` is
///    below `nr_node_ids` before `do_get_mempolicy()` is entered at all, so
///    even `MPOL_F_MEMS_ALLOWED` reports it.
/// 2. `do_get_mempolicy()` rejects an unknown flag bit or an out-of-range
///    combination (only `MPOL_F_MEMS_ALLOWED`, `MPOL_F_ADDR` and `MPOL_F_NODE`
///    are accepted together).
/// 3. `MPOL_F_MEMS_ALLOWED` returns immediately with the allowed set; it skips
///    the address check, but not the wrapper's `maxnode` check above.
/// 4. `MPOL_F_ADDR` selects by address; a non-zero `addr` without it is
///    `EINVAL`.
pub fn validate_get(
    flags: usize,
    nodemask_supplied: bool,
    maxnode: usize,
    addr: usize,
    nr_node_ids: usize,
) -> Result<(), GetMempolicyError> {
    if nodemask_supplied && maxnode < nr_node_ids {
        return Err(GetMempolicyError::NodeMaskTooShort);
    }
    if flags & !(MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) != 0 {
        return Err(GetMempolicyError::InvalidFlags);
    }
    if flags & MPOL_F_MEMS_ALLOWED != 0 {
        if flags & (MPOL_F_NODE | MPOL_F_ADDR) != 0 {
            return Err(GetMempolicyError::InvalidFlags);
        }
        return Ok(());
    }
    if flags & MPOL_F_ADDR == 0 && addr != 0 {
        return Err(GetMempolicyError::UnexpectedAddress);
    }
    Ok(())
}

/// `do_get_mempolicy()`'s `MPOL_F_NODE` rule.
///
/// Without `MPOL_F_ADDR`, `MPOL_F_NODE` asks for the *next* interleave node.
/// Linux returns `EINVAL` unless the queried policy is the task's own
/// `MPOL_INTERLEAVE` or `MPOL_WEIGHTED_INTERLEAVE`; a non-interleave policy has
/// no "next node" to report. TheKernel has one node and never advances the
/// interleave cursor, so an admitted interleave policy always reports node 0.
pub const fn next_interleave_node(mode: u32, is_task_policy: bool) -> Option<usize> {
    if !is_task_policy {
        return None;
    }
    match mode {
        MPOL_INTERLEAVE | MPOL_WEIGHTED_INTERLEAVE => Some(0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NR_NODE_IDS: usize = 1;
    const ALLOWED: usize = ALLOWED_NODEMASK;

    fn set(mode: u32) -> Result<MempolicyRequest, MempolicyError> {
        validate(mode, 0, false, ALLOWED)
    }

    #[test]
    fn sanitize_mode_flags_reports_before_the_mask_is_read() {
        assert_eq!(
            sanitize_mode_flags(MPOL_BIND | MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES),
            Err(MempolicyError::InvalidMode)
        );
        assert_eq!(
            sanitize_mode_flags(MPOL_MAX),
            Err(MempolicyError::InvalidMode)
        );
        assert_eq!(
            sanitize_mode_flags(MPOL_BIND | MPOL_F_NUMA_BALANCING),
            Ok((MPOL_BIND, MPOL_F_NUMA_BALANCING))
        );
        assert_eq!(
            sanitize_mode_flags(MPOL_BIND | MPOL_F_STATIC_NODES),
            Ok((MPOL_BIND, MPOL_F_STATIC_NODES))
        );
    }

    #[test]
    fn static_and_relative_nodes_are_mutually_exclusive_before_the_mask() {
        // `sanitize_mpol_flags()` runs before `get_nodes()`, so even a bogus
        // `nmask` reports the mode error.
        assert_eq!(
            validate(
                MPOL_BIND | MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES,
                0,
                false,
                ALLOWED
            ),
            Err(MempolicyError::InvalidMode)
        );
        assert_eq!(
            validate(
                MPOL_DEFAULT | MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES,
                0,
                false,
                ALLOWED
            ),
            Err(MempolicyError::InvalidMode)
        );
    }

    #[test]
    fn mode_table_matches_linux_mpol_max() {
        for mode in 0..MPOL_MAX {
            let result = validate(mode, 1, true, ALLOWED);
            match mode {
                MPOL_DEFAULT | MPOL_LOCAL => {
                    assert_eq!(result, Err(MempolicyError::EmptyNodeMask), "mode {mode}");
                }
                _ => assert!(result.is_ok(), "mode {mode} => {result:?}"),
            }
        }
        assert_eq!(set(MPOL_MAX), Err(MempolicyError::InvalidMode));
        assert_eq!(set(u32::MAX), Err(MempolicyError::InvalidMode));
    }

    #[test]
    fn modes_without_a_mask_follow_linux_mpol_new() {
        assert!(validate(MPOL_DEFAULT, 0, false, ALLOWED).is_ok());
        assert!(validate(MPOL_LOCAL, 0, false, ALLOWED).is_ok());
        // MPOL_PREFERRED degenerates to MPOL_LOCAL, which is admitted.
        assert!(validate(MPOL_PREFERRED, 0, false, ALLOWED).is_ok());
        for mode in [
            MPOL_BIND,
            MPOL_INTERLEAVE,
            MPOL_PREFERRED_MANY,
            MPOL_WEIGHTED_INTERLEAVE,
        ] {
            assert_eq!(
                validate(mode, 0, false, ALLOWED),
                Err(MempolicyError::EmptyNodeMask),
                "mode {mode}"
            );
        }
    }

    #[test]
    fn preferred_with_relatives_needs_a_mask_but_plain_does_not() {
        assert!(validate(MPOL_PREFERRED, 0, false, ALLOWED).is_ok());
        assert_eq!(
            validate(MPOL_PREFERRED | MPOL_F_STATIC_NODES, 0, false, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
        assert_eq!(
            validate(MPOL_PREFERRED | MPOL_F_RELATIVE_NODES, 0, false, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
        // A mask naming only a node outside the allowed set intersects to
        // nothing. `mpol_new()` only inspects the *user* mask when it decides
        // whether an empty `MPOL_PREFERRED` becomes `MPOL_LOCAL`, but the
        // surviving preferred policy is then created by
        // `mpol_new_preferred()` from the intersected mask, which rejects an
        // empty result -- so this is EINVAL with or without a user-nodemask
        // flag (Linux v7.2.3 `mm/mempolicy.c` `mpol_new()` +
        // `mpol_set_nodemask()`).
        assert_eq!(
            validate(MPOL_PREFERRED, 2, true, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
        assert_eq!(
            validate(MPOL_PREFERRED | MPOL_F_STATIC_NODES, 2, true, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
        // A non-empty intersection still narrows to its first allowed node.
        assert_eq!(
            effective_policy(validate(MPOL_PREFERRED, 3, true, ALLOWED).unwrap()),
            (MPOL_PREFERRED, 1)
        );
    }

    #[test]
    fn reported_policy_keeps_the_mode_flags() {
        // `*policy |= (pol->flags & MPOL_MODE_FLAGS)`.
        assert_eq!(
            reported_policy(MPOL_BIND, MPOL_F_NUMA_BALANCING),
            MPOL_BIND | MPOL_F_NUMA_BALANCING
        );
        assert_eq!(
            reported_policy(MPOL_INTERLEAVE, MPOL_F_RELATIVE_NODES),
            MPOL_INTERLEAVE | MPOL_F_RELATIVE_NODES
        );
        assert_eq!(reported_policy(MPOL_LOCAL, 0), MPOL_LOCAL);
        // `MPOL_F_NODE`/`MPOL_F_ADDR`/`MPOL_F_MEMS_ALLOWED` are query flags,
        // not mode flags, so they can never appear in the reported policy.
        assert_eq!(reported_policy(MPOL_BIND, MPOL_F_NODE as u32), MPOL_BIND);
        // Only STATIC/RELATIVE make Linux store and report `w.user_nodemask`.
        assert!(stores_user_nodemask(MPOL_F_STATIC_NODES));
        assert!(stores_user_nodemask(MPOL_F_RELATIVE_NODES));
        assert!(!stores_user_nodemask(MPOL_F_NUMA_BALANCING));
        assert!(!stores_user_nodemask(0));
    }

    #[test]
    fn local_rejects_a_mask_and_user_nodemask_flags() {
        assert!(validate(MPOL_LOCAL, 0, false, ALLOWED).is_ok());
        assert_eq!(
            validate(MPOL_LOCAL, 1, true, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
        assert_eq!(
            validate(MPOL_LOCAL | MPOL_F_STATIC_NODES, 0, false, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
    }

    #[test]
    fn default_rejects_a_mask_even_when_empty_of_allowed_nodes() {
        assert!(validate(MPOL_DEFAULT, 0, false, ALLOWED).is_ok());
        assert_eq!(
            validate(MPOL_DEFAULT, 2, true, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
    }

    #[test]
    fn numa_balancing_only_admits_bind_and_preferred_many() {
        for mode in [MPOL_BIND, MPOL_PREFERRED_MANY] {
            assert!(
                validate(mode | MPOL_F_NUMA_BALANCING, 1, true, ALLOWED).is_ok(),
                "mode {mode}"
            );
        }
        for mode in [
            MPOL_DEFAULT,
            MPOL_PREFERRED,
            MPOL_INTERLEAVE,
            MPOL_LOCAL,
            MPOL_WEIGHTED_INTERLEAVE,
        ] {
            assert_eq!(
                validate(mode | MPOL_F_NUMA_BALANCING, 0, false, ALLOWED),
                Err(MempolicyError::InvalidMode),
                "mode {mode}"
            );
        }
    }

    #[test]
    fn all_four_nodemask_modes_are_admitted_on_the_single_allowed_node() {
        // TheKernel is single-node: these must be accepted exactly where Linux
        // accepts them, and degenerate to node 0.
        for mode in [
            MPOL_BIND,
            MPOL_INTERLEAVE,
            MPOL_PREFERRED_MANY,
            MPOL_WEIGHTED_INTERLEAVE,
        ] {
            let request =
                validate(mode, 1, true, ALLOWED).unwrap_or_else(|e| panic!("{mode}: {e:?}"));
            assert_eq!(request.mode, mode);
            assert_eq!(request.nodes, 1);
            assert_eq!(effective_policy(request), (mode, 1));
        }
    }

    #[test]
    fn get_nodes_bounds_are_linux_page_size_times_bits_per_byte() {
        // `maxnode` is a bit count, so `maxnode == 2` is the smallest value
        // that can carry node 0. 0 and 1 are the "no mask" spellings.
        assert_eq!(parse_node_mask(1, &[0], true), Ok((0, false)));
        assert_eq!(parse_node_mask(0, &[0], true), Ok((0, false)));
        assert_eq!(parse_node_mask(2, &[1], true), Ok((1, true)));
        // A NULL mask skips the size check entirely.
        assert_eq!(parse_node_mask(usize::MAX, &[0], false), Ok((0, false)));
        // `bits` is `maxnode - 1`, compared against `PAGE_SIZE * BITS_PER_BYTE`
        // before anything is read: 32768 bits is admitted, 32769 is EINVAL.
        assert_eq!(
            parse_node_mask(MAX_NODEMASK_BITS + 1, &[1], true),
            Ok((1, true))
        );
        assert_eq!(
            parse_node_mask(MAX_NODEMASK_BITS + 2, &[1], true),
            Err(MempolicyError::NodeMaskTooLong)
        );
    }

    #[test]
    fn scanned_words_bounds_what_a_caller_must_read() {
        assert_eq!(scanned_words(0), 0);
        assert_eq!(scanned_words(1), 0);
        assert_eq!(scanned_words(2), 1);
        assert_eq!(scanned_words(65), 1);
        assert_eq!(scanned_words(66), 2);
        assert_eq!(scanned_words(513), 8);
        assert_eq!(
            scanned_words(MAX_NODEMASK_BITS + 2),
            MAX_NODEMASK_BITS.div_ceil(usize::BITS as usize) + 1
        );
    }

    #[test]
    fn get_nodes_keeps_only_bits_below_maxnode_in_the_first_word() {
        // `maxnode == 2` names node 0 only, so bit 1 is outside the window and
        // `get_bitmap()` masks it off before the kernel ever sees it.
        assert_eq!(parse_node_mask(2, &[3, 0], true), Ok((1, true)));
        // Raising `maxnode` brings bit 1 into the window, where it names a node
        // TheKernel does not have; `mpol_set_nodemask()` is what rejects it.
        assert_eq!(parse_node_mask(3, &[3, 0], true), Ok((3, true)));
    }

    #[test]
    fn get_nodes_rejects_a_set_bit_above_max_numnodes() {
        // `maxnode == 66` leaves 65 bits, so word 1 is read and a set bit there
        // is above `MAX_NUMNODES` (64): EINVAL.
        assert_eq!(
            parse_node_mask(66, &[1, 1], true),
            Err(MempolicyError::NodeOutOfRange)
        );
        assert_eq!(parse_node_mask(66, &[1, 0], true), Ok((1, true)));
        assert_eq!(parse_node_mask(66, &[0, 0], true), Ok((0, true)));
        // `maxnode == 65` leaves exactly 64 bits, so only word 0 is scanned and
        // the trailing word is never examined.
        assert_eq!(parse_node_mask(65, &[1, 1], true), Ok((1, true)));
        // A 513-bit window naming bit 100 is the same rejection.
        let mut words = [0usize; 8];
        words[1] = 1 << (100 - 64);
        assert_eq!(
            parse_node_mask(513, &words, true),
            Err(MempolicyError::NodeOutOfRange)
        );
        let mut words = [0usize; 8];
        words[0] = 1;
        assert_eq!(parse_node_mask(513, &words, true), Ok((1, true)));
    }

    #[test]
    fn mbind_rejects_an_unaligned_address_instead_of_rounding_it_down() {
        assert_eq!(
            plan_mbind(0x1001, 0x1000, MPOL_BIND, 0, false),
            Err(MempolicyError::UnalignedStart)
        );
        assert_eq!(
            plan_mbind(0x1000, 0x1000, MPOL_BIND, 0, false),
            Ok(Some(MbindPlan {
                start: 0x1000,
                len: 0x1000,
                flags: 0
            }))
        );
    }

    #[test]
    fn mbind_rejects_unknown_flags_and_move_all_without_capability() {
        assert_eq!(
            plan_mbind(0x1000, 0x1000, MPOL_BIND, 1 << 3, true),
            Err(MempolicyError::InvalidMode)
        );
        assert_eq!(
            plan_mbind(0x1000, 0x1000, MPOL_BIND, MPOL_MF_MOVE_ALL, false),
            Err(MempolicyError::MoveAllNotPermitted)
        );
        assert!(plan_mbind(0x1000, 0x1000, MPOL_BIND, MPOL_MF_MOVE_ALL, true).is_ok());
    }

    #[test]
    fn mbind_rejects_a_wrapping_range_but_accepts_an_empty_one() {
        // `end < start` after page alignment is EINVAL. `usize::MAX` itself
        // aligns to zero (an empty range), so the wrapping case needs a length
        // that stays aligned once rounded up.
        assert_eq!(
            plan_mbind(0x1000, usize::MAX - 0xfff, MPOL_BIND, 0, false),
            Err(MempolicyError::RangeOverflow)
        );
        assert_eq!(
            plan_mbind(0x1000, usize::MAX, MPOL_BIND, 0, false),
            Ok(None)
        );
        // A zero length is a successful no-op, and the address must still be
        // aligned: Linux checks alignment before the empty-range return.
        assert_eq!(plan_mbind(0x1000, 0, MPOL_BIND, 0, false), Ok(None));
        assert_eq!(
            plan_mbind(0x1001, 0, MPOL_BIND, 0, false),
            Err(MempolicyError::UnalignedStart)
        );
        // A length below one page still covers a whole page.
        assert_eq!(
            plan_mbind(0x1000, 1, MPOL_BIND, 0, false),
            Ok(Some(MbindPlan {
                start: 0x1000,
                len: 0x1000,
                flags: 0
            }))
        );
    }

    #[test]
    fn mbind_drops_strict_for_the_default_policy() {
        let plan = plan_mbind(0x1000, 0x1000, MPOL_DEFAULT, MPOL_MF_STRICT, false)
            .unwrap()
            .unwrap();
        assert_eq!(plan.flags, 0);
        let plan = plan_mbind(0x1000, 0x1000, MPOL_BIND, MPOL_MF_STRICT, false)
            .unwrap()
            .unwrap();
        assert_eq!(plan.flags, MPOL_MF_STRICT);
    }

    #[test]
    fn get_mempolicy_rejects_unknown_flags() {
        // `kernel_get_mempolicy()`'s `maxnode` check runs before
        // `do_get_mempolicy()` reads the flags, so a supplied mask below
        // `nr_node_ids` masks an unknown flag with the wrapper's own error.
        assert_eq!(
            validate_get(1 << 3, true, 0, 0xdead, 1),
            Err(GetMempolicyError::NodeMaskTooShort)
        );
        assert_eq!(
            validate_get(1 << 3, false, 0, 0xdead, 1),
            Err(GetMempolicyError::InvalidFlags)
        );
        assert_eq!(
            validate_get(MPOL_F_MEMS_ALLOWED | MPOL_F_NODE, false, 0, 0, 1),
            Err(GetMempolicyError::InvalidFlags)
        );
        assert_eq!(
            validate_get(MPOL_F_MEMS_ALLOWED | MPOL_F_ADDR, false, 0, 0, 1),
            Err(GetMempolicyError::InvalidFlags)
        );
    }

    #[test]
    fn mems_allowed_skips_the_address_check_but_not_the_wrapper_maxnode_check() {
        // The `maxnode` check is in `kernel_get_mempolicy()`, *before*
        // `do_get_mempolicy()` can take its `MPOL_F_MEMS_ALLOWED` early return,
        // while the address check lives inside `do_get_mempolicy()` and is
        // therefore skipped.
        assert_eq!(
            validate_get(MPOL_F_MEMS_ALLOWED, true, 0, 0xdead, NR_NODE_IDS),
            Err(GetMempolicyError::NodeMaskTooShort)
        );
        assert_eq!(
            validate_get(
                MPOL_F_MEMS_ALLOWED,
                true,
                NR_NODE_IDS,
                0xdead,
                NR_NODE_IDS
            ),
            Ok(())
        );
        assert_eq!(
            validate_get(MPOL_F_MEMS_ALLOWED, false, 0, 0, NR_NODE_IDS),
            Ok(())
        );
    }

    #[test]
    fn a_supplied_nodemask_is_rejected_below_nr_node_ids() {
        assert_eq!(
            validate_get(0, true, 0, 0, NR_NODE_IDS),
            Err(GetMempolicyError::NodeMaskTooShort)
        );
        assert_eq!(validate_get(0, true, 1, 0, NR_NODE_IDS), Ok(()));
        assert_eq!(validate_get(0, false, 0, 0, NR_NODE_IDS), Ok(()));
        // The maxnode check precedes the address check.
        assert_eq!(
            validate_get(0, true, 0, 0xdead, NR_NODE_IDS),
            Err(GetMempolicyError::NodeMaskTooShort)
        );
        assert_eq!(
            validate_get(0, false, 0, 0xdead, NR_NODE_IDS),
            Err(GetMempolicyError::UnexpectedAddress)
        );
    }

    #[test]
    fn f_node_without_f_addr_requires_the_tasks_own_interleave_policy() {
        assert_eq!(next_interleave_node(MPOL_INTERLEAVE, true), Some(0));
        assert_eq!(
            next_interleave_node(MPOL_WEIGHTED_INTERLEAVE, true),
            Some(0)
        );
        assert_eq!(next_interleave_node(MPOL_BIND, true), None);
        assert_eq!(next_interleave_node(MPOL_DEFAULT, true), None);
        assert_eq!(next_interleave_node(MPOL_LOCAL, true), None);
        assert_eq!(next_interleave_node(MPOL_INTERLEAVE, false), None);
    }

    #[test]
    fn effective_policy_narrows_preferred_to_its_first_node() {
        let request = validate(MPOL_PREFERRED, 1, true, ALLOWED).unwrap();
        assert_eq!(effective_policy(request), (MPOL_PREFERRED, 1));
        // An empty preferred list becomes MPOL_LOCAL, as `mpol_new()` does.
        let request = validate(MPOL_PREFERRED, 0, false, ALLOWED).unwrap();
        assert_eq!(effective_policy(request), (MPOL_LOCAL, 0));
    }

    #[test]
    fn relative_nodes_remap_onto_the_allowed_set() {
        // The relative remap makes the *n*-th user bit name the *n*-th allowed
        // node, so a mask that names no literal allowed node still admits.
        let request = validate(MPOL_BIND | MPOL_F_RELATIVE_NODES, 0b1000, true, ALLOWED).unwrap();
        assert_eq!(request.nodes, ALLOWED);
        assert_eq!(request.user_nodes, 0b1000);
        // A static mask is intersected literally, so an out-of-set bit alone is
        // rejected by the mode's own emptiness check.
        assert_eq!(
            validate(MPOL_BIND | MPOL_F_STATIC_NODES, 0b1000, true, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
        // A mask that is empty for the user is empty under either reading.
        assert_eq!(
            validate(MPOL_BIND | MPOL_F_RELATIVE_NODES, 0, true, ALLOWED),
            Err(MempolicyError::EmptyNodeMask)
        );
    }
}
