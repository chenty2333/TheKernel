// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. Port of the contract of Linux 7.2.3
// arch/x86/mm/pat/memtype_interval.c (INTERVAL_TREE over [start, end)).
//! Set of possibly overlapping half-open physical intervals.
//!
//! Linux keeps reservations in an augmented red-black tree and answers
//! "every entry overlapping [a, b)". This set keeps the same contract with an
//! ordered map keyed by start address. Overlap queries visit entries whose
//! start is below the query end, which is bounded by the number of live
//! reservations (a few dozen for graphics BARs and mmaps), not by the number of
//! pages.

use alloc::{collections::BTreeMap, vec::Vec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval<T> {
    pub start: u64,
    /// Exclusive end.
    pub end: u64,
    pub value: T,
}

#[derive(Clone, Debug)]
pub struct IntervalSet<T> {
    /// Keyed by `(start, sequence)` so equal starts can coexist.
    entries: BTreeMap<(u64, u64), Interval<T>>,
    next_seq: u64,
}

impl<T: Copy> Default for IntervalSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> IntervalSet<T> {
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_seq: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert `[start, end)` without checking for overlap.
    pub fn insert(&mut self, start: u64, end: u64, value: T) {
        debug_assert!(start < end);
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries
            .insert((start, seq), Interval { start, end, value });
    }

    /// Every entry that overlaps `[start, end)`, in ascending start order.
    pub fn overlapping(&self, start: u64, end: u64) -> Vec<Interval<T>> {
        self.entries
            .range(..(end, 0))
            .map(|(_, e)| *e)
            .filter(|e| e.end > start)
            .collect()
    }

    /// First entry overlapping `[start, end)`, as Linux `interval_iter_first`.
    pub fn first_overlapping(&self, start: u64, end: u64) -> Option<Interval<T>> {
        self.entries
            .range(..(end, 0))
            .map(|(_, e)| *e)
            .find(|e| e.end > start)
    }

    /// Remove and return the entry whose range is exactly `[start, end)`,
    /// as Linux `memtype_erase` does. Returns `None` if there is no exact match.
    pub fn remove_exact(&mut self, start: u64, end: u64) -> Option<Interval<T>> {
        let key = self
            .entries
            .range((start, 0)..(start.saturating_add(1), 0))
            .find(|(_, e)| e.end == end)
            .map(|(k, _)| *k)?;
        self.entries.remove(&key)
    }

    /// Iterate all entries in ascending start order.
    pub fn iter(&self) -> impl Iterator<Item = &Interval<T>> {
        self.entries.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_query_is_half_open() {
        let mut s = IntervalSet::new();
        s.insert(0x1000, 0x2000, 1u8);
        s.insert(0x3000, 0x4000, 2u8);
        assert_eq!(s.overlapping(0x2000, 0x3000).len(), 0);
        assert_eq!(s.overlapping(0x1fff, 0x2001).len(), 1);
        assert_eq!(s.overlapping(0x0, 0x10000).len(), 2);
        assert_eq!(s.first_overlapping(0x3fff, 0x4000).map(|e| e.value), Some(2));
    }

    #[test]
    fn equal_starts_coexist_and_erase_matches_exactly() {
        let mut s = IntervalSet::new();
        s.insert(0x1000, 0x2000, 1u8);
        s.insert(0x1000, 0x3000, 2u8);
        assert_eq!(s.overlapping(0x1000, 0x1001).len(), 2);
        assert_eq!(s.remove_exact(0x1000, 0x2500), None);
        assert_eq!(s.remove_exact(0x1000, 0x3000).map(|e| e.value), Some(2));
        assert_eq!(s.len(), 1);
    }
}
