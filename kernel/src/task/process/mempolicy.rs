//! `task::process` subsections; see the parent `mod.rs` for the module map.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mempolicy {
    pub mode: u32,
    /// `pol->flags & MPOL_MODE_FLAGS`: the shaping flags the caller supplied
    /// (`MPOL_F_STATIC_NODES`, `MPOL_F_RELATIVE_NODES`,
    /// `MPOL_F_NUMA_BALANCING`). `get_mempolicy(2)` reports them ORed into the
    /// policy word, and a user-nodemask flag also selects which stored mask is
    /// authoritative.
    pub mode_flags: u32,
    pub nodemask: usize,
    /// `pol->w.user_nodemask`: the caller's mask exactly as supplied.
    ///
    /// `mpol_set_nodemask()` stores it only for a policy carrying a
    /// user-nodemask flag, and `do_get_mempolicy()` then reports it instead of
    /// the intersected `pol->nodes`. It is 0 for every other policy.
    pub user_nodemask: usize,
    /// Preferred allocation node for `MPOL_BIND` and `MPOL_PREFERRED_MANY`.
    ///
    /// `None` is Linux's `NUMA_NO_NODE`: no home-node preference has been
    /// configured for this policy.
    pub home_node: Option<usize>,
}

impl Mempolicy {
    pub const fn new(mode: u32, nodemask: usize) -> Self {
        Self {
            mode,
            mode_flags: 0,
            nodemask,
            user_nodemask: 0,
            home_node: None,
        }
    }

    /// Records the sanitized mode flags and the caller's own mask, the two
    /// `get_mempolicy(2)` report inputs that the stored `pol->nodes` cannot
    /// reconstruct.
    pub const fn with_request_flags(mut self, mode_flags: u32, user_nodemask: usize) -> Self {
        self.mode_flags = mode_flags;
        self.user_nodemask = user_nodemask;
        self
    }

    pub const fn with_home_node(mut self, home_node: usize) -> Self {
        self.home_node = Some(home_node);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MempolicyRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) policy: Mempolicy,
}

#[derive(Clone, Debug)]
pub(crate) struct MempolicyState {
pub(crate)     process_policy: Mempolicy,
pub(crate)     ranges: Vec<MempolicyRange>,
}

/// Immutable NUMA-policy view bound to one process image.
///
/// `/proc/PID/numa_maps` keeps this snapshot together with its pinned address
/// space so a later exec cannot pair the old VMAs with the new image's policy
/// state (or the inverse).
#[derive(Clone, Debug)]
pub(crate) struct MempolicySnapshot {
pub(crate)     process_policy: Mempolicy,
pub(crate)     ranges: Vec<MempolicyRange>,
}

impl MempolicySnapshot {
pub(crate) fn policy_for_addr(&self, addr: usize) -> Mempolicy {
        self.ranges
            .iter()
            .rev()
            .find(|range| addr >= range.start && addr < range.end)
            .map_or(self.process_policy, |range| range.policy)
    }
}

impl Default for MempolicyState {
    fn default() -> Self {
        Self {
            process_policy: Mempolicy::new(0, 0),
            ranges: Vec::new(),
        }
    }
}

impl MempolicyState {
    fn first_node(mask: usize) -> Option<usize> {
        (mask != 0).then(|| mask.trailing_zeros() as usize)
    }

    fn node_mask(node: usize) -> usize {
        1usize.checked_shl(node as u32).unwrap_or(0)
    }

    fn node_ordinal(mask: usize, node: usize) -> Option<usize> {
        let mut ordinal = 0usize;
        for candidate in 0..usize::BITS as usize {
            if mask & Self::node_mask(candidate) == 0 {
                continue;
            }
            if candidate == node {
                return Some(ordinal);
            }
            ordinal += 1;
        }
        None
    }

    fn nth_node(mask: usize, ordinal: usize) -> Option<usize> {
        let mut seen = 0usize;
        for candidate in 0..usize::BITS as usize {
            if mask & Self::node_mask(candidate) == 0 {
                continue;
            }
            if seen == ordinal {
                return Some(candidate);
            }
            seen += 1;
        }
        None
    }

    fn migration_destination(
        old_mask: usize,
        new_mask: usize,
        source_node: usize,
    ) -> Option<usize> {
        let source_mask = Self::node_mask(source_node);
        if old_mask & source_mask == 0 || new_mask == 0 {
            return None;
        }

        if old_mask == new_mask {
            return None;
        }

        if old_mask.count_ones() == new_mask.count_ones() {
            return Self::node_ordinal(old_mask, source_node)
                .and_then(|ordinal| Self::nth_node(new_mask, ordinal));
        }

        if new_mask & source_mask != 0 {
            return None;
        }

        Self::first_node(new_mask)
    }

    fn migrate_policy(policy: &mut Mempolicy, old_mask: usize, new_mask: usize) -> bool {
        let source_node = Self::first_node(policy.nodemask).unwrap_or(0);
        let Some(dest_node) = Self::migration_destination(old_mask, new_mask, source_node) else {
            return false;
        };
        if dest_node == source_node {
            return false;
        }
        policy.nodemask = Self::node_mask(dest_node);
        true
    }

pub(crate) fn migrate_ranges(&mut self, old_mask: usize, new_mask: usize) -> usize {
        let mut migrated = 0;
        for range in &mut self.ranges {
            migrated += usize::from(Self::migrate_policy(&mut range.policy, old_mask, new_mask));
        }
        migrated
    }

pub(crate) fn remove_range(&mut self, start: usize, end: usize) {
        let old_ranges = core::mem::take(&mut self.ranges);
        for range in old_ranges {
            if range.end <= start || range.start >= end {
                self.ranges.push(range);
                continue;
            }
            if range.start < start {
                self.ranges.push(MempolicyRange {
                    start: range.start,
                    end: start,
                    policy: range.policy,
                });
            }
            if range.end > end {
                self.ranges.push(MempolicyRange {
                    start: end,
                    end: range.end,
                    policy: range.policy,
                });
            }
        }
    }

    /// Linux represents an `mbind(MPOL_DEFAULT)` range by removing its VMA
    /// policy rather than by recording a synthetic default-policy interval.
pub(crate) fn bind_range(&mut self, start: usize, end: usize, policy: Mempolicy) {
        self.remove_range(start, end);
        if policy.mode != linux_raw_sys::mempolicy::MPOL_DEFAULT as u32 {
            self.ranges.push(MempolicyRange { start, end, policy });
        }
    }

pub(crate) fn policy_for_addr(&self, addr: usize) -> Option<Mempolicy> {
        self.ranges
            .iter()
            .rev()
            .find(|range| addr >= range.start && addr < range.end)
            .map(|range| range.policy)
    }

    /// Applies a home node to policy intervals intersecting `start..end`.
    ///
    /// `mbind` policy intervals can be narrower than an address-space VMA, so
    /// this operates on the interval topology rather than treating the VMA's
    /// first policy as covering the whole VMA.  The sorted traversal provides
    /// Linux's address-order partial-update behavior if a later policy is not
    /// supported by `set_mempolicy_home_node`.
pub(crate) fn try_set_home_node_in_range(
        old_ranges: &[MempolicyRange],
        start: usize,
        end: usize,
        home_node: usize,
    ) -> AxResult<(Vec<MempolicyRange>, bool, Option<axerrno::LinuxError>)> {
        let capacity = old_ranges.len().checked_mul(3).ok_or(AxError::NoMemory)?;
        let mut new_ranges = Vec::new();
        new_ranges
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        let mut old_ranges_sorted = Vec::new();
        old_ranges_sorted
            .try_reserve_exact(old_ranges.len())
            .map_err(|_| AxError::NoMemory)?;
        old_ranges_sorted.extend(old_ranges.iter().copied());
        let mut old_ranges = old_ranges_sorted;
        old_ranges.sort_unstable_by_key(|range| range.start);
        let mut updated = false;

        let mut ranges = old_ranges.into_iter();
        while let Some(range) = ranges.next() {
            if range.end <= start || range.start >= end {
                new_ranges.push(range);
                continue;
            }
            if range.policy.mode != linux_raw_sys::mempolicy::MPOL_BIND as u32
                && range.policy.mode != linux_raw_sys::mempolicy::MPOL_PREFERRED_MANY as u32
            {
                new_ranges.push(range);
                new_ranges.extend(ranges);
                return Ok((new_ranges, updated, Some(axerrno::LinuxError::EOPNOTSUPP)));
            }

            let overlap_start = range.start.max(start);
            let overlap_end = range.end.min(end);
            if range.start < overlap_start {
                new_ranges.push(MempolicyRange {
                    start: range.start,
                    end: overlap_start,
                    policy: range.policy,
                });
            }
            let mut policy = range.policy;
            policy.home_node = Some(home_node);
            new_ranges.push(MempolicyRange {
                start: overlap_start,
                end: overlap_end,
                policy,
            });
            if overlap_end < range.end {
                new_ranges.push(MempolicyRange {
                    start: overlap_end,
                    end: range.end,
                    policy: range.policy,
                });
            }
            updated = true;
        }
        Ok((new_ranges, updated, None))
    }
}
