// SPDX-License-Identifier: Apache-2.0
//! Address-ordered AVL nodes with maximum free-length summaries.

use alloc::boxed::Box;
use core::{cmp::Ordering, ops::Range};

pub(super) type Link = Option<Box<GapNode>>;

#[derive(Clone)]
pub(super) struct GapNode {
    pub(super) range: Range<usize>,
    pub(super) left: Link,
    pub(super) right: Link,
    pub(super) max_length: usize,
    height: u8,
}

impl GapNode {
    pub(super) fn insert(root: Link, range: Range<usize>) -> Box<Self> {
        let Some(mut root) = root else {
            return Box::new(Self {
                max_length: range.end - range.start,
                range,
                left: None,
                right: None,
                height: 1,
            });
        };
        match range.start.cmp(&root.range.start) {
            Ordering::Less => root.left = Some(Self::insert(root.left.take(), range)),
            Ordering::Greater => root.right = Some(Self::insert(root.right.take(), range)),
            Ordering::Equal => root.range = range,
        }
        root.balance()
    }

    pub(super) fn remove(root: Link, start: usize) -> Link {
        let mut root = root?;
        match start.cmp(&root.range.start) {
            Ordering::Less => root.left = Self::remove(root.left.take(), start),
            Ordering::Greater => root.right = Self::remove(root.right.take(), start),
            Ordering::Equal => {
                let Some(right) = root.right.take() else {
                    return root.left;
                };
                let (right, mut replacement) = Self::take_first(right);
                replacement.left = root.left;
                replacement.right = right;
                return Some(replacement.balance());
            }
        }
        Some(root.balance())
    }

    pub(super) fn predecessor(root: &Link, start: usize) -> Option<Range<usize>> {
        let mut current = root.as_deref();
        let mut result = None;
        while let Some(node) = current {
            if node.range.start <= start {
                result = Some(node.range.clone());
                current = node.right.as_deref();
            } else {
                current = node.left.as_deref();
            }
        }
        result
    }

    pub(super) fn successor(root: &Link, start: usize) -> Option<Range<usize>> {
        let mut current = root.as_deref();
        let mut result = None;
        while let Some(node) = current {
            if node.range.start >= start {
                result = Some(node.range.clone());
                current = node.left.as_deref();
            } else {
                current = node.right.as_deref();
            }
        }
        result
    }

    fn take_first(mut root: Box<Self>) -> (Link, Box<Self>) {
        let Some(left) = root.left.take() else {
            return (root.right.take(), root);
        };
        let (left, first) = Self::take_first(left);
        root.left = left;
        (Some(root.balance()), first)
    }

    fn balance(mut self: Box<Self>) -> Box<Self> {
        self.refresh();
        let difference = self.balance_factor();
        if difference > 1 {
            // A height difference greater than one proves this child exists.
            let left = self
                .left
                .as_mut()
                .expect("left-heavy AVL node has a left child");
            if left.balance_factor() < 0 {
                self.left = self.left.take().map(Self::rotate_left);
            }
            self.rotate_right()
        } else if difference < -1 {
            let right = self
                .right
                .as_mut()
                .expect("right-heavy AVL node has a right child");
            if right.balance_factor() > 0 {
                self.right = self.right.take().map(Self::rotate_right);
            }
            self.rotate_left()
        } else {
            self
        }
    }

    fn rotate_left(mut self: Box<Self>) -> Box<Self> {
        let mut pivot = self
            .right
            .take()
            .expect("left rotation requires a right child");
        self.right = pivot.left.take();
        self.refresh();
        pivot.left = Some(self);
        pivot.refresh();
        pivot
    }

    fn rotate_right(mut self: Box<Self>) -> Box<Self> {
        let mut pivot = self
            .left
            .take()
            .expect("right rotation requires a left child");
        self.left = pivot.right.take();
        self.refresh();
        pivot.right = Some(self);
        pivot.refresh();
        pivot
    }

    fn refresh(&mut self) {
        self.height = 1 + Self::height(&self.left).max(Self::height(&self.right));
        self.max_length = (self.range.end - self.range.start)
            .max(self.left.as_ref().map_or(0, |node| node.max_length))
            .max(self.right.as_ref().map_or(0, |node| node.max_length));
    }

    fn balance_factor(&self) -> i16 {
        i16::from(Self::height(&self.left)) - i16::from(Self::height(&self.right))
    }

    fn height(root: &Link) -> u8 {
        root.as_ref().map_or(0, |node| node.height)
    }

    #[cfg(test)]
    pub(super) fn check(root: &Link, bounds: Range<usize>) -> (u8, usize, usize) {
        let Some(node) = root else {
            return (0, 0, 0);
        };
        assert!(bounds.start <= node.range.start && node.range.end <= bounds.end);
        assert!(node.range.start < node.range.end);
        let (left_height, left_max, left_count) =
            Self::check(&node.left, bounds.start..node.range.start);
        let (right_height, right_max, right_count) =
            Self::check(&node.right, node.range.end..bounds.end);
        assert!(left_height.abs_diff(right_height) <= 1);
        assert_eq!(node.height, 1 + left_height.max(right_height));
        assert_eq!(
            node.max_length,
            (node.range.end - node.range.start)
                .max(left_max)
                .max(right_max)
        );
        (node.height, node.max_length, 1 + left_count + right_count)
    }
}
