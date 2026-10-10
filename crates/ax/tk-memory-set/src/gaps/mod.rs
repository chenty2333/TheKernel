// SPDX-License-Identifier: Apache-2.0
//! Derived free-range index. The owning MemorySet publishes area and gap changes
//! under the same mutable borrow; callers cannot mutate this index separately.

use core::ops::Range;

mod search;
mod tree;

use search::GapSearch;
use tree::{GapNode, Link};

#[derive(Clone)]
pub(super) struct GapIndex {
    root: Link,
    // The unbounded free suffix needs no allocation, including for new sets.
    tail_start: usize,
}

impl GapIndex {
    pub(super) const fn new() -> Self {
        Self {
            root: None,
            tail_start: 0,
        }
    }

    /// Builds an index from the ordered occupied ranges in a replacement tree.
    pub(super) fn from_areas(ranges: impl IntoIterator<Item = Range<usize>>) -> Self {
        let mut index = Self::new();
        for range in ranges {
            index.occupy(range);
        }
        index
    }

    pub(super) fn find(
        &self,
        lower: usize,
        upper: usize,
        size: usize,
        align: usize,
    ) -> Option<usize> {
        let search = GapSearch::new(lower, upper, size, align)?;
        search
            .in_tree(self.root.as_deref())
            .or_else(|| search.in_gap(self.tail_start..usize::MAX))
    }

    /// Removes a successfully installed mapping from one free interval.
    pub(super) fn occupy(&mut self, range: Range<usize>) {
        debug_assert!(range.start < range.end);
        if range.start >= self.tail_start {
            self.insert(self.tail_start..range.start);
            self.tail_start = range.end;
            return;
        }

        let gap = GapNode::predecessor(&self.root, range.start)
            .expect("installed area must belong to a free gap");
        assert!(
            range.end <= gap.end,
            "installed area crosses an occupied range"
        );
        self.root = GapNode::remove(self.root.take(), gap.start);
        self.insert(gap.start..range.start);
        self.insert(range.end..gap.end);
    }

    /// Adds free coverage, coalescing adjacent intervals and the implicit tail.
    /// Releasing an already free portion is allowed during range reconciliation.
    pub(super) fn release(&mut self, mut range: Range<usize>) {
        debug_assert!(range.start < range.end);
        if range.end >= self.tail_start {
            range.start = range.start.min(self.tail_start);
        }
        if let Some(previous) = GapNode::predecessor(&self.root, range.start) {
            if previous.end >= range.start {
                range.start = previous.start;
                range.end = range.end.max(previous.end);
                self.root = GapNode::remove(self.root.take(), previous.start);
            }
        }
        while let Some(next) = GapNode::successor(&self.root, range.start) {
            if next.start > range.end {
                break;
            }
            range.end = range.end.max(next.end);
            self.root = GapNode::remove(self.root.take(), next.start);
        }
        if range.end >= self.tail_start {
            self.tail_start = range.start;
        } else {
            self.insert(range);
        }
    }

    fn insert(&mut self, range: Range<usize>) {
        if range.start < range.end {
            self.root = Some(GapNode::insert(self.root.take(), range));
        }
    }
}

#[cfg(test)]
mod tests;
