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
    /// A word `get_bitmap()` must read is absent from the supplied window.
    ///
    /// `get_bitmap()` copies out of user memory and reports `-EFAULT` when that
    /// copy fails; the pure contract cannot read memory, so a caller that hands
    /// over fewer words than `scanned_words(maxnode)` names reports the same
    /// condition here. `read_nodemask()` faults the window in first and always
    /// supplies the complete window, so this is a caller-contract failure
    /// rather than a user-visible errno. `EFAULT`.
    NodeMaskUnreadable,
    /// `MPOL_MF_MOVE_ALL` was requested without `CAP_SYS_NICE`. `EPERM`.
    MoveAllNotPermitted,
    /// `start` is not page-aligned. `EINVAL`.
    UnalignedStart,
    /// The page-aligned range wraps past the end of the address space. `EINVAL`.
    RangeOverflow,
}

/// `get_bitmap()` in Linux v7.2.3 `mm/mempolicy.c`.
///
/// ```c
/// static int get_bitmap(unsigned long *mask, const unsigned long __user *nmask,
/// 		      unsigned long maxnode)
/// {
/// 	unsigned long nlongs = BITS_TO_LONGS(maxnode);
/// 	int ret;
///
/// 	if (in_compat_syscall())
/// 		ret = compat_get_bitmap(mask, ...);
/// 	else
/// 		ret = copy_from_user(mask, nmask,
/// 				     nlongs * sizeof(unsigned long));
///
/// 	if (ret)
/// 		return -EFAULT;
///
/// 	if (maxnode % BITS_PER_LONG)
/// 		mask[nlongs - 1] &= (1UL << (maxnode % BITS_PER_LONG)) - 1;
///
/// 	return 0;
/// }
/// ```
///
/// Only the final partial word is trimmed, and only when the bit count is not
/// itself a multiple of `BITS_PER_LONG`; every other word is read whole. With
/// `MAX_NUMNODES == usize::BITS` this configuration's `nodemask_t` is one word
/// and `get_nodes()` never asks for a window wider than `BITS_PER_LONG`, so the
/// copy is always the single word `word`.
fn get_bitmap_word(word: usize, maxnode: usize) -> usize {
    let partial = maxnode % usize::BITS as usize;
    if partial == 0 {
        word
    } else {
        word & mask_below(partial)
    }
}

/// `get_nodes()` in Linux v7.2.3 `mm/mempolicy.c`.
///
/// ```c
/// static int get_nodes(nodemask_t *nodes, const unsigned long __user *nmask,
/// 		     unsigned long maxnode)
/// {
/// 	--maxnode;
/// 	nodes_clear(*nodes);
/// 	if (maxnode == 0 || !nmask)
/// 		return 0;
/// 	if (maxnode > PAGE_SIZE*BITS_PER_BYTE)
/// 		return -EINVAL;
///
/// 	/*
/// 	 * When the user specified more nodes than supported just check
/// 	 * if the non supported part is all zero, one word at a time,
/// 	 * starting at the end.
/// 	 */
/// 	while (maxnode > MAX_NUMNODES) {
/// 		unsigned long bits = min_t(unsigned long, maxnode, BITS_PER_LONG);
/// 		unsigned long t;
///
/// 		if (get_bitmap(&t, &nmask[(maxnode - 1) / BITS_PER_LONG], bits))
/// 			return -EFAULT;
///
/// 		if (maxnode - bits >= MAX_NUMNODES) {
/// 			maxnode -= bits;
/// 		} else {
/// 			maxnode = MAX_NUMNODES;
/// 			t &= ~((1UL << (MAX_NUMNODES % BITS_PER_LONG)) - 1);
/// 		}
/// 		if (t)
/// 			return -EINVAL;
/// 	}
///
/// 	return get_bitmap(nodes_addr(*nodes), nmask, maxnode);
/// }
/// ```
///
/// `maxnode` is a count of bits, so a mask naming *n* nodes passes *n + 1*.
/// Linux decrements first, which makes the decrement wrap for `maxnode == 0`:
/// `maxnode` of 1 is the "no mask" spelling, while 0 leaves `ULONG_MAX` and is
/// rejected by the length bound. A NULL `nmask` is "no mask" for every
/// `maxnode`.
///
/// Otherwise the words from the top of the window down to word 0 are examined.
/// Words at or above `MAX_NUMNODES` must be zero and are read *whole*: the
/// `t &= ~((1UL << (MAX_NUMNODES % BITS_PER_LONG)) - 1)` clamp is a no-op when
/// `MAX_NUMNODES` is a multiple of `BITS_PER_LONG`, so a set bit anywhere in
/// the word that holds the window's top — including one above the caller's own
/// `maxnode` — is `EINVAL`. Only the final `get_bitmap()` window (at most
/// `MAX_NUMNODES` bits, starting at word 0) is trimmed to `maxnode % 64` bits.
///
/// Returns the accepted mask together with whether `nmask` was supplied,
/// because `mpol_new()` distinguishes "no nodes" from "node 0".
pub fn parse_node_mask(
    maxnode: usize,
    words: &[usize],
    nmask_supplied: bool,
) -> Result<(usize, bool), MempolicyError> {
    // `--maxnode; if (maxnode == 0 || !nmask) return 0;`
    let mut maxnode = maxnode.wrapping_sub(1);
    if maxnode == 0 || !nmask_supplied {
        return Ok((0, false));
    }
    // Linux only rejects the size when a mask was actually supplied, and it
    // does so on the decremented value: 32769 is the largest admitted window.
    if maxnode > MAX_NODEMASK_BITS {
        return Err(MempolicyError::NodeMaskTooLong);
    }

    // `while (maxnode > MAX_NUMNODES)`: `bits` is `min(maxnode, BITS_PER_LONG)`,
    // which can only be `BITS_PER_LONG` while `maxnode` is larger, so every
    // window this loop reads is one whole word at `(maxnode - 1) / BITS_PER_LONG`
    // and `get_bitmap()` trims nothing.
    while maxnode > MAX_NUMNODES {
        let bits = maxnode.min(usize::BITS as usize);
        let index = (maxnode - 1) / usize::BITS as usize;
        let word = words
            .get(index)
            .copied()
            .ok_or(MempolicyError::NodeMaskUnreadable)?;
        let mut t = get_bitmap_word(word, bits);
        if maxnode - bits >= MAX_NUMNODES {
            maxnode -= bits;
        } else {
            maxnode = MAX_NUMNODES;
            t &= !mask_below(MAX_NUMNODES % usize::BITS as usize);
        }
        if rejects_scanned_word(index, t) {
            return Err(MempolicyError::NodeOutOfRange);
        }
    }

    // The accepted mask is `get_bitmap(nodes_addr(*nodes), nmask, maxnode)`,
    // which for `maxnode <= MAX_NUMNODES == BITS_PER_LONG` copies word 0 alone
    // and trims it to the caller's window.
    let word = words
        .first()
        .copied()
        .ok_or(MempolicyError::NodeMaskUnreadable)?;
    Ok((get_bitmap_word(word, maxnode), true))
}

/// `(1 << bits) - 1`, with `bits == 64` saturated to all ones.
const fn mask_below(bits: usize) -> usize {
    if bits >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << bits) - 1
    }
}

/// `get_nodes()`'s in-loop `if (t) return -EINVAL;` for the word just read at
/// `index` (`mm/mempolicy.c:1673-1688`).
///
/// The scan reads the caller's window from its highest word down to word 0, and
/// that order is observable: the check runs *before* the next lower word is
/// read, so a set bit above `MAX_NUMNODES` in a high word is `EINVAL` even when
/// a lower word of the same window is unreadable — the lower word is never
/// read, and `EFAULT` never happens. Word 0 is the accepted mask rather than a
/// check (`get_bitmap(nodes_addr(*nodes), nmask, maxnode)`), and the loop body
/// only runs while `maxnode > MAX_NUMNODES`, so `index` is never 0 there.
///
/// `checked_value` is the value Linux tests: the raw word after the iteration's
/// `t &= ~(...)` clamp. The clamp only masks a partial top word when
/// `MAX_NUMNODES` is not a multiple of `BITS_PER_LONG`, which cannot happen
/// here (`MAX_NUMNODES == BITS_PER_LONG == 64`).
pub const fn rejects_scanned_word(index: usize, checked_value: usize) -> bool {
    index > 0 && checked_value != 0
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

/// The policy `mpol_new()`/`mpol_set_nodemask()` actually built, which is the
/// value Linux stores and `get_mempolicy(2)` later reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveMempolicy {
    /// `pol->mode`.
    pub mode: u32,
    /// `pol->flags & MPOL_MODE_FLAGS`.
    pub mode_flags: u32,
    /// `pol->nodes`, the request narrowed to the allowed node set.
    pub nodes: usize,
    /// `pol->w.user_nodemask`, reported when `mpol_store_user_nodemask()`
    /// holds.
    pub user_nodemask: usize,
}

/// The `MPOL_DEFAULT`/`MPOL_PREFERRED`/`MPOL_LOCAL` normalisation performed by
/// `mpol_new()`.
///
/// Linux rewrites an empty `MPOL_PREFERRED` into `MPOL_LOCAL`, and
/// `mpol_new_preferred()` narrows a preferred list to its first node. Both are
/// observable through `get_mempolicy(2)`, so the kernel stores the result of
/// this function rather than the raw request.
///
/// `MPOL_DEFAULT` is the third case, and the reason the whole policy is
/// returned rather than only its mode: `mpol_new()` returns `NULL` for it
/// (`mm/mempolicy.c:446-450`, *not* a policy with default fields), so
/// `do_set_mempolicy()` stores no policy at all (`mm/mempolicy.c:1091-1092`)
/// and `do_get_mempolicy()` reports `&default_policy`, whose `flags` are zero
/// (`mm/mempolicy.c:1186-1187`, `mm/mempolicy.c:1219-1225`). The caller's mode
/// flags are therefore *dropped*:
/// `set_mempolicy(MPOL_DEFAULT | MPOL_F_STATIC_NODES, NULL, 0)` succeeds and
/// the following `get_mempolicy(2)` reports a plain `MPOL_DEFAULT`. The mask is
/// already empty for this mode, because `mpol_new()` rejects a non-empty node
/// list with `-EINVAL`.
pub const fn effective_policy(request: MempolicyRequest) -> EffectiveMempolicy {
    if request.mode == MPOL_DEFAULT {
        return EffectiveMempolicy {
            mode: MPOL_DEFAULT,
            mode_flags: 0,
            nodes: 0,
            user_nodemask: 0,
        };
    }
    if request.mode == MPOL_PREFERRED {
        if !request.has_nodes || request.nodes == 0 {
            return EffectiveMempolicy {
                mode: MPOL_LOCAL,
                mode_flags: request.mode_flags,
                nodes: 0,
                user_nodemask: request.user_nodes,
            };
        }
        // `first_node()` is the lowest set bit.
        return EffectiveMempolicy {
            mode: MPOL_PREFERRED,
            mode_flags: request.mode_flags,
            nodes: request.nodes & request.nodes.wrapping_neg(),
            user_nodemask: request.user_nodes,
        };
    }
    EffectiveMempolicy {
        mode: request.mode,
        mode_flags: request.mode_flags,
        nodes: request.nodes,
        user_nodemask: request.user_nodes,
    }
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
            EffectiveMempolicy {
                mode: MPOL_PREFERRED,
                mode_flags: 0,
                nodes: 1,
                user_nodemask: 3,
            }
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
            assert_eq!(
                effective_policy(request),
                EffectiveMempolicy {
                    mode,
                    mode_flags: 0,
                    nodes: 1,
                    user_nodemask: 1,
                }
            );
        }
    }

    #[test]
    fn get_nodes_bounds_are_linux_page_size_times_bits_per_byte() {
        // `maxnode` is a bit count, so `maxnode == 2` is the smallest value
        // that can carry node 0, while `maxnode == 1` leaves zero after the
        // decrement and is the "no mask" spelling.
        assert_eq!(parse_node_mask(1, &[0], true), Ok((0, false)));
        assert_eq!(parse_node_mask(2, &[1], true), Ok((1, true)));
        // `--maxnode` wraps to `ULONG_MAX` for `maxnode == 0`, which is not the
        // zero the "no mask" test looks for: a supplied mask with `maxnode == 0`
        // fails the length bound with EINVAL (`mm/mempolicy.c:1663-1667`).
        assert_eq!(
            parse_node_mask(0, &[0], true),
            Err(MempolicyError::NodeMaskTooLong)
        );
        assert_eq!(parse_node_mask(0, &[0], false), Ok((0, false)));
        // A NULL mask skips the size check entirely.
        assert_eq!(parse_node_mask(usize::MAX, &[0], false), Ok((0, false)));
        // The bound is on the decremented value, before any word is read:
        // 32769 bits is admitted (and needs all 512 words `get_bitmap()` would
        // read), 32770 is EINVAL.
        let mut window = [0usize; MAX_NODEMASK_BITS.div_ceil(usize::BITS as usize)];
        window[0] = 1;
        assert_eq!(
            parse_node_mask(MAX_NODEMASK_BITS + 1, &window, true),
            Ok((1, true))
        );
        assert_eq!(
            parse_node_mask(MAX_NODEMASK_BITS + 2, &[1], true),
            Err(MempolicyError::NodeMaskTooLong)
        );
        // A window shorter than `get_bitmap()` would read is a contract
        // violation, not a silent zero-fill.
        assert_eq!(
            parse_node_mask(MAX_NODEMASK_BITS + 1, &[1], true),
            Err(MempolicyError::NodeMaskUnreadable)
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
    fn get_nodes_checks_the_whole_top_word_not_only_bits_below_maxnode() {
        // `maxnode == 102` leaves 101 bits. The top window is word 1, read
        // whole because `bits = min(101, BITS_PER_LONG) == BITS_PER_LONG`, and
        // `MAX_NUMNODES % BITS_PER_LONG == 0` makes the `t &= ~(...)` clamp a
        // no-op -- so a set bit at position 40 of that word is a node above
        // `MAX_NUMNODES` even though it also sits above the caller's `maxnode`
        // of 101. `get_nodes()` returns EINVAL; trimming the word to the
        // caller's window first would accept `{0, 1 << 40}` as "node 0".
        assert_eq!(
            parse_node_mask(102, &[0, 1usize << 40], true),
            Err(MempolicyError::NodeOutOfRange)
        );
        // The same window with the high bit clear keeps word 0's node 0.
        assert_eq!(parse_node_mask(102, &[0, 0], true), Ok((0, true)));
        assert_eq!(parse_node_mask(102, &[1, 0], true), Ok((1, true)));
        // Bits 64..101 are above `MAX_NUMNODES`, so any of them set is EINVAL.
        assert_eq!(
            parse_node_mask(102, &[1, 1], true),
            Err(MempolicyError::NodeOutOfRange)
        );
        // A window that stops at `MAX_NUMNODES` never reads word 1 at all.
        assert_eq!(parse_node_mask(65, &[1, 1], true), Ok((1, true)));
    }

    #[test]
    fn scanned_word_rejection_applies_only_above_word_zero() {
        // The loop's `if (t)` runs for every word the scan reads above
        // `MAX_NUMNODES`, and those words are never word 0: word 0 is the
        // accepted mask the final `get_bitmap()` copies.
        assert!(rejects_scanned_word(1, 1));
        assert!(rejects_scanned_word(512, 1usize << 63));
        assert!(!rejects_scanned_word(1, 0));
        assert!(!rejects_scanned_word(0, 1));
        assert!(!rejects_scanned_word(0, usize::MAX));
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
        assert_eq!(
            effective_policy(request),
            EffectiveMempolicy {
                mode: MPOL_PREFERRED,
                mode_flags: 0,
                nodes: 1,
                user_nodemask: 1,
            }
        );
        // An empty preferred list becomes MPOL_LOCAL, as `mpol_new()` does.
        let request = validate(MPOL_PREFERRED, 0, false, ALLOWED).unwrap();
        assert_eq!(
            effective_policy(request),
            EffectiveMempolicy {
                mode: MPOL_LOCAL,
                mode_flags: 0,
                nodes: 0,
                user_nodemask: 0,
            }
        );
    }

    #[test]
    fn effective_default_policy_drops_the_callers_shaping_flags() {
        // `mpol_new()` returns NULL for MPOL_DEFAULT, so the mode flags the
        // caller supplied never reach a `struct mempolicy` and
        // `do_get_mempolicy()` reports plain `MPOL_DEFAULT`
        // (`mm/mempolicy.c:446-450`, `mm/mempolicy.c:1219-1225`). A
        // `MPOL_F_STATIC_NODES`/`MPOL_F_RELATIVE_NODES` bit is therefore not
        // observable after a successful default request.
        for flags in [0, MPOL_F_STATIC_NODES, MPOL_F_RELATIVE_NODES] {
            let request = validate(MPOL_DEFAULT | flags, 0, false, ALLOWED).unwrap();
            assert_eq!(
                effective_policy(request),
                EffectiveMempolicy {
                    mode: MPOL_DEFAULT,
                    mode_flags: 0,
                    nodes: 0,
                    user_nodemask: 0,
                },
                "flags {flags:#x}"
            );
        }
        // The two user-nodemask flags together never reach `mpol_new()`:
        // `sanitize_mpol_flags()` rejects the combination first.
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
