// SPDX-License-Identifier: Apache-2.0
//! First-fit lookup with checked half-open bounds and subtree-size pruning.

use core::ops::Range;

use super::tree::GapNode;

pub(super) struct GapSearch {
    lower: usize,
    upper: usize,
    size: usize,
    align: usize,
    #[cfg(test)]
    pub(super) visited: core::cell::Cell<usize>,
}

impl GapSearch {
    pub(super) fn new(lower: usize, upper: usize, size: usize, align: usize) -> Option<Self> {
        if size == 0 || !align.is_power_of_two() || !size.is_multiple_of(align) {
            return None;
        }
        if lower.checked_add(size)? > upper {
            return None;
        }
        Some(Self {
            lower,
            upper,
            size,
            align,
            #[cfg(test)]
            visited: core::cell::Cell::new(0),
        })
    }

    pub(super) fn in_tree(&self, root: Option<&GapNode>) -> Option<usize> {
        let node = root?;
        #[cfg(test)]
        self.visited.set(self.visited.get() + 1);

        if node.max_length < self.size {
            return None;
        }
        if node.range.end <= self.lower {
            return self.in_tree(node.right.as_deref());
        }
        if node.range.start >= self.upper {
            return self.in_tree(node.left.as_deref());
        }
        self.in_tree(node.left.as_deref())
            .or_else(|| self.in_gap(node.range.clone()))
            .or_else(|| self.in_tree(node.right.as_deref()))
    }

    pub(super) fn in_gap(&self, range: Range<usize>) -> Option<usize> {
        let start = range.start.max(self.lower).checked_add(self.align - 1)? & !(self.align - 1);
        (start.checked_add(self.size)? <= range.end.min(self.upper)).then_some(start)
    }
}
