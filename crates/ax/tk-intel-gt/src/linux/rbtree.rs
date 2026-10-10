// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
// Linux v7.2.3 include/linux/rbtree.h and lib/rbtree.c operations.

#![allow(unsafe_code)]

use crate::intel_engine_cs_upstream::{RbNode, RbRoot, RbRootCached};

/// Linux `RB_ROOT` initializer for an empty red-black tree.
#[allow(non_upper_case_globals)]
pub const RB_ROOT: RbRoot = RbRoot {
    node: core::ptr::null_mut(),
};

/// Linux `RB_EMPTY_ROOT` predicate.
#[inline]
#[allow(non_snake_case)]
pub fn RB_EMPTY_ROOT(root: &RbRoot) -> bool {
    root.node.is_null()
}

pub trait RbNodePointer {
    fn as_rb_node_ptr(self) -> *mut RbNode;
}

impl RbNodePointer for *mut RbNode {
    fn as_rb_node_ptr(self) -> *mut RbNode {
        self
    }
}

impl RbNodePointer for &mut RbNode {
    fn as_rb_node_ptr(self) -> *mut RbNode {
        self
    }
}

impl RbNodePointer for &RbNode {
    fn as_rb_node_ptr(self) -> *mut RbNode {
        self as *const RbNode as *mut RbNode
    }
}

#[allow(non_snake_case)]
pub unsafe fn RB_EMPTY_NODE<N: RbNodePointer>(node: N) -> bool {
    let node = node.as_rb_node_ptr();
    assert!(!node.is_null());
    unsafe { (*node).parent_color == node as usize }
}

#[allow(non_snake_case)]
pub unsafe fn RB_CLEAR_NODE<N: RbNodePointer>(node: N) {
    let node = node.as_rb_node_ptr();
    assert!(!node.is_null());
    unsafe { (*node).parent_color = node as usize };
}

const RB_RED: usize = 0;
const RB_BLACK: usize = 1;
const RB_COLOR_MASK: usize = 1;

#[inline]
unsafe fn parent(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        core::ptr::null_mut()
    } else {
        ((*node).parent_color & !3) as *mut RbNode
    }
}

#[inline]
unsafe fn color(node: *mut RbNode) -> usize {
    if node.is_null() {
        RB_BLACK
    } else {
        (*node).parent_color & RB_COLOR_MASK
    }
}

#[inline]
unsafe fn set_parent_color(node: *mut RbNode, p: *mut RbNode, c: usize) {
    if !node.is_null() {
        (*node).parent_color = (p as usize & !3) | c;
    }
}

#[inline]
unsafe fn set_parent(node: *mut RbNode, p: *mut RbNode) {
    if !node.is_null() {
        (*node).parent_color = (p as usize & !3) | color(node);
    }
}

#[inline]
unsafe fn set_color(node: *mut RbNode, c: usize) {
    if !node.is_null() {
        (*node).parent_color = ((*node).parent_color & !RB_COLOR_MASK) | c;
    }
}

unsafe fn replace_node(root: *mut *mut RbNode, old: *mut RbNode, new: *mut RbNode) {
    let p = parent(old);
    if p.is_null() {
        *root = new;
    } else if core::ptr::eq((*p).left, old) {
        (*p).left = new;
    } else {
        (*p).right = new;
    }
    set_parent(new, p);
}

/// Augmentation hooks for trees whose nodes carry a subtree summary, as Linux
/// `struct rb_augment_callbacks`. `propagate(node, stop)` recomputes the summary
/// from `node` upward until `stop` (or the root); `copy(old, new)` gives `new`
/// the summary of `old`; `rotate(old, new)` is called after a rotation that
/// moved `new` above `old`.
#[repr(C)]
pub struct RbAugment {
    pub propagate: unsafe fn(*mut RbNode, *mut RbNode),
    pub copy: unsafe fn(*mut RbNode, *mut RbNode),
    pub rotate: unsafe fn(*mut RbNode, *mut RbNode),
}

#[inline]
unsafe fn augment_rotate(aug: *const RbAugment, old: *mut RbNode, new: *mut RbNode) {
    if !aug.is_null() {
        unsafe { ((*aug).rotate)(old, new) };
    }
}

unsafe fn rotate_left(root: *mut *mut RbNode, node: *mut RbNode, aug: *const RbAugment) {
    let right = (*node).right;
    (*node).right = (*right).left;
    set_parent((*right).left, node);
    replace_node(root, node, right);
    (*right).left = node;
    set_parent(node, right);
    unsafe { augment_rotate(aug, node, right) };
}

unsafe fn rotate_right(root: *mut *mut RbNode, node: *mut RbNode, aug: *const RbAugment) {
    let left = (*node).left;
    (*node).left = (*left).right;
    set_parent((*left).right, node);
    replace_node(root, node, left);
    (*left).right = node;
    set_parent(node, left);
    unsafe { augment_rotate(aug, node, left) };
}

/// Link a node into an ordinary Linux rb-tree at a caller-selected slot.
///
/// This is the `rb_link_node()` header operation; the node is inserted red and
/// must be followed by `rb_insert_color()` before it is observed by readers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rb_link_node(
    node: *mut RbNode,
    parent: *mut RbNode,
    link: *mut *mut RbNode,
) {
    unsafe {
        (*node).parent_color = parent as usize;
        (*node).left = core::ptr::null_mut();
        (*node).right = core::ptr::null_mut();
        *link = node;
    }
}

/// Rebalance an ordinary rb-tree after `rb_link_node()`.
///
/// This is an independently implemented LinuxKPI primitive. The caller owns
/// ordering and node lifetime; this function only restores red-black
/// invariants using the crate's intrusive-node representation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rb_insert_color(node: *mut RbNode, root: *mut RbRoot) {
    unsafe { insert_color(node, root, core::ptr::null()) };
}

/// Insert fixup for a node already linked by `rb_link_node()`, reporting
/// rotations to `aug` when it is non-null (Linux `rb_insert_augmented()`).
pub unsafe fn insert_color(node: *mut RbNode, root: *mut RbRoot, aug: *const RbAugment) {
    let mut node = node;
    let root = unsafe { core::ptr::addr_of_mut!((*root).node) };

    loop {
        let p = unsafe { parent(node) };
        if p.is_null() || unsafe { color(p) == RB_BLACK } {
            break;
        }

        let g = unsafe { parent(p) };
        if g.is_null() {
            break;
        }

        if core::ptr::eq(p, unsafe { (*g).left }) {
            let u = unsafe { (*g).right };
            if unsafe { color(u) == RB_RED } {
                unsafe {
                    set_color(p, RB_BLACK);
                    set_color(u, RB_BLACK);
                    set_color(g, RB_RED);
                }
                node = g;
                continue;
            }

            if core::ptr::eq(node, unsafe { (*p).right }) {
                node = p;
                unsafe { rotate_left(root, node, aug) };
            }
            let p = unsafe { parent(node) };
            let g = unsafe { parent(p) };
            unsafe {
                set_color(p, RB_BLACK);
                set_color(g, RB_RED);
                rotate_right(root, g, aug);
            }
        } else {
            let u = unsafe { (*g).left };
            if unsafe { color(u) == RB_RED } {
                unsafe {
                    set_color(p, RB_BLACK);
                    set_color(u, RB_BLACK);
                    set_color(g, RB_RED);
                }
                node = g;
                continue;
            }

            if core::ptr::eq(node, unsafe { (*p).left }) {
                node = p;
                unsafe { rotate_right(root, node, aug) };
            }
            let p = unsafe { parent(node) };
            let g = unsafe { parent(p) };
            unsafe {
                set_color(p, RB_BLACK);
                set_color(g, RB_RED);
                rotate_left(root, g, aug);
            }
        }
        break;
    }

    unsafe { set_color(*root, RB_BLACK) };
}

pub unsafe fn rb_next(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        return core::ptr::null_mut();
    }
    if !(*node).right.is_null() {
        let mut next = (*node).right;
        while !(*next).left.is_null() {
            next = (*next).left;
        }
        return next;
    }
    let mut current = node;
    let mut p = parent(current);
    while !p.is_null() && core::ptr::eq((*p).right, current) {
        current = p;
        p = parent(p);
    }
    p
}

/// Return the first node in in-order traversal of a Linux rb-tree.
/// This is the `rb_first()` inline from `include/linux/rbtree.h`.
#[inline]
pub unsafe fn rb_first(root: *const RbRoot) -> *mut RbNode {
    if root.is_null() {
        return core::ptr::null_mut();
    }
    let mut node = unsafe { (*root).node };
    while !node.is_null() {
        let left = unsafe { (*node).left };
        if left.is_null() {
            break;
        }
        node = left;
    }
    node
}

unsafe fn erase_fixup(root: *mut *mut RbNode, mut node: *mut RbNode, mut p: *mut RbNode, aug: *const RbAugment) {
    while !core::ptr::eq(node, *root) && color(node) == RB_BLACK {
        if p.is_null() {
            break;
        }
        if core::ptr::eq(node, (*p).left) {
            let mut sibling = (*p).right;
            if color(sibling) == RB_RED {
                set_color(sibling, RB_BLACK);
                set_color(p, RB_RED);
                rotate_left(root, p, aug);
                sibling = (*p).right;
            }
            if sibling.is_null()
                || (color((*sibling).left) == RB_BLACK && color((*sibling).right) == RB_BLACK)
            {
                set_color(sibling, RB_RED);
                node = p;
                p = parent(node);
            } else {
                if color((*sibling).right) == RB_BLACK {
                    set_color((*sibling).left, RB_BLACK);
                    set_color(sibling, RB_RED);
                    rotate_right(root, sibling, aug);
                    sibling = (*p).right;
                }
                set_color(sibling, color(p));
                set_color(p, RB_BLACK);
                set_color((*sibling).right, RB_BLACK);
                rotate_left(root, p, aug);
                node = *root;
                p = core::ptr::null_mut();
            }
        } else {
            let mut sibling = (*p).left;
            if color(sibling) == RB_RED {
                set_color(sibling, RB_BLACK);
                set_color(p, RB_RED);
                rotate_right(root, p, aug);
                sibling = (*p).left;
            }
            if sibling.is_null()
                || (color((*sibling).right) == RB_BLACK && color((*sibling).left) == RB_BLACK)
            {
                set_color(sibling, RB_RED);
                node = p;
                p = parent(node);
            } else {
                if color((*sibling).left) == RB_BLACK {
                    set_color((*sibling).right, RB_BLACK);
                    set_color(sibling, RB_RED);
                    rotate_left(root, sibling, aug);
                    sibling = (*p).left;
                }
                set_color(sibling, color(p));
                set_color(p, RB_BLACK);
                set_color((*sibling).left, RB_BLACK);
                rotate_right(root, p, aug);
                node = *root;
                p = core::ptr::null_mut();
            }
        }
    }
    set_color(node, RB_BLACK);
}

unsafe fn erase(node: *mut RbNode, root: *mut *mut RbNode, aug: *const RbAugment) {
    let mut moved = node;
    let mut moved_color = color(moved);
    let child;
    let child_parent;

    if (*node).left.is_null() {
        child = (*node).right;
        child_parent = parent(node);
        replace_node(root, node, child);
    } else if (*node).right.is_null() {
        child = (*node).left;
        child_parent = parent(node);
        replace_node(root, node, child);
    } else {
        moved = (*node).right;
        while !(*moved).left.is_null() {
            moved = (*moved).left;
        }
        moved_color = color(moved);
        child = (*moved).right;
        let old_parent = parent(moved);
        if core::ptr::eq(old_parent, node) {
            child_parent = moved;
            set_parent(child, moved);
        } else {
            child_parent = old_parent;
            replace_node(root, moved, child);
            (*moved).right = (*node).right;
            set_parent((*moved).right, moved);
        }
        replace_node(root, node, moved);
        (*moved).left = (*node).left;
        set_parent((*moved).left, moved);
        set_color(moved, color(node));
    }

    if moved_color == RB_BLACK {
        erase_fixup(root, child, child_parent, aug);
    }
    if !aug.is_null() {
        // Structure changed around `child_parent` and `moved`; recompute the
        // summaries on both paths to the root (exact, independent of shape).
        unsafe {
            if !child_parent.is_null() {
                ((*aug).propagate)(child_parent, core::ptr::null_mut());
            }
            if !moved.is_null() && moved != node {
                ((*aug).propagate)(moved, core::ptr::null_mut());
            }
        }
    }
    (*node).left = core::ptr::null_mut();
    (*node).right = core::ptr::null_mut();
    (*node).parent_color = 0;
}

pub fn rb_first_cached(root: &RbRootCached) -> *mut RbNode {
    root.leftmost
}

pub trait RbNodePtr {
    fn rb_node_ptr(self) -> *mut RbNode;
}
impl RbNodePtr for *mut RbNode {
    fn rb_node_ptr(self) -> *mut RbNode {
        self
    }
}
impl RbNodePtr for &mut RbNode {
    fn rb_node_ptr(self) -> *mut RbNode {
        self
    }
}
impl RbNodePtr for &RbNode {
    fn rb_node_ptr(self) -> *mut RbNode {
        (self as *const RbNode).cast_mut()
    }
}

/// Remove a node from an ordinary Linux rb_root.
pub unsafe fn rb_erase_impl<N: RbNodePtr>(node: N, root: &mut RbRoot) {
    let node = node.rb_node_ptr();
    assert!(!node.is_null());
    unsafe { erase(node, core::ptr::addr_of_mut!(root.node), core::ptr::null()) };
}

pub fn rb_erase_cached<N: RbNodePtr>(node: N, root: &mut RbRootCached) {
    let node = node.rb_node_ptr();
    assert!(!node.is_null());
    unsafe {
        if core::ptr::eq(root.leftmost, node) {
            root.leftmost = rb_next(node);
        }
        erase(node, core::ptr::addr_of_mut!(root.root.node), core::ptr::null());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct Entry {
        node: RbNode,
        key: u32,
    }

    #[test]
    fn c_abi_insert_balances_and_preserves_inorder_sequence() {
        let mut entries = [
            Entry { node: RbNode { parent_color: 0, left: core::ptr::null_mut(), right: core::ptr::null_mut() }, key: 3 },
            Entry { node: RbNode { parent_color: 0, left: core::ptr::null_mut(), right: core::ptr::null_mut() }, key: 1 },
            Entry { node: RbNode { parent_color: 0, left: core::ptr::null_mut(), right: core::ptr::null_mut() }, key: 4 },
            Entry { node: RbNode { parent_color: 0, left: core::ptr::null_mut(), right: core::ptr::null_mut() }, key: 0 },
            Entry { node: RbNode { parent_color: 0, left: core::ptr::null_mut(), right: core::ptr::null_mut() }, key: 2 },
        ];
        let mut root = RbRoot { node: core::ptr::null_mut() };

        for entry in &mut entries {
            let node = core::ptr::addr_of_mut!(entry.node);
            let mut parent = core::ptr::null_mut();
            let mut link = core::ptr::addr_of_mut!(root.node);
            unsafe {
                while !(*link).is_null() {
                    parent = *link;
                    let existing = parent.cast::<Entry>();
                    link = if entry.key < (*existing).key {
                        core::ptr::addr_of_mut!((*parent).left)
                    } else {
                        core::ptr::addr_of_mut!((*parent).right)
                    };
                }
                rb_link_node(node, parent, link);
                rb_insert_color(node, &mut root);
            }
        }

        assert_eq!(unsafe { color(root.node) }, RB_BLACK);
        let mut node = unsafe { rb_first(&root) };
        let mut observed = [u32::MAX; 5];
        for key in &mut observed {
            assert!(!node.is_null());
            *key = unsafe { (*node.cast::<Entry>()).key };
            node = unsafe { rb_next(node) };
        }
        assert_eq!(observed, [0, 1, 2, 3, 4]);
        assert!(node.is_null());
    }

    #[test]
    fn cached_first_tracks_erase_of_leftmost_node() {
        let mut first = RbNode {
            parent_color: 0,
            left: core::ptr::null_mut(),
            right: core::ptr::null_mut(),
        };
        let mut second = RbNode {
            parent_color: &mut first as *mut _ as usize,
            left: core::ptr::null_mut(),
            right: core::ptr::null_mut(),
        };
        first.right = &mut second;
        let mut root = RbRootCached {
            root: crate::intel_engine_cs_upstream::RbRoot { node: &mut first },
            leftmost: &mut first,
        };
        assert!(core::ptr::eq(rb_first_cached(&root), &mut first));
        rb_erase_cached(&mut first, &mut root);
        assert!(core::ptr::eq(root.leftmost, &mut second));
        assert!(core::ptr::eq(root.root.node, &mut second));
    }
}

/// Linux `rb_erase()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rb_erase(node: *mut RbNode, root: *mut RbRoot) {
    unsafe { self::rb_erase_impl(node, &mut *root) };
}

/// Linux `rb_insert_augmented()`: rebalance after `rb_link_node()`, keeping the
/// subtree summaries in `aug` current across rotations.
///
/// # Safety
/// `node` must be linked into `root` and `aug` must describe the node type.
pub unsafe fn rb_insert_augmented(node: *mut RbNode, root: *mut RbRoot, aug: *const RbAugment) {
    unsafe { insert_color(node, root, aug) };
}

/// Linux `rb_insert_augmented_cached()` with `rb_insert_color_cached()`'s
/// leftmost tracking.
///
/// # Safety
/// As [`rb_insert_augmented`]; `leftmost` must be true iff `node` is the new
/// leftmost node.
pub unsafe fn rb_insert_augmented_cached(
    node: *mut RbNode,
    root: *mut RbRootCached,
    leftmost: bool,
    aug: *const RbAugment,
) {
    unsafe {
        if leftmost {
            (*root).leftmost = node;
        }
        insert_color(node, core::ptr::addr_of_mut!((*root).root), aug);
    }
}

/// Linux `rb_erase_augmented()`.
///
/// # Safety
/// `node` must be in `root`; `aug` must describe the node type.
pub unsafe fn rb_erase_augmented(node: *mut RbNode, root: *mut RbRoot, aug: *const RbAugment) {
    unsafe { erase(node, core::ptr::addr_of_mut!((*root).node), aug) };
}

/// Linux `rb_erase_augmented_cached()`: also keeps the cached leftmost node.
///
/// # Safety
/// As [`rb_erase_augmented`].
pub unsafe fn rb_erase_augmented_cached(
    node: *mut RbNode,
    root: *mut RbRootCached,
    aug: *const RbAugment,
) {
    unsafe {
        if (*root).leftmost == node {
            (*root).leftmost = rb_next(node);
        }
        erase(node, core::ptr::addr_of_mut!((*root).root.node), aug);
    }
}

/// Linux `rb_parent()`.
///
/// # Safety
/// `node` must be a valid node.
pub unsafe fn rb_parent(node: *mut RbNode) -> *mut RbNode {
    unsafe { parent(node) }
}

/// Linux `rb_prev()`: the in-order predecessor, or NULL.
///
/// # Safety
/// `node` must be a node of a valid tree.
pub unsafe fn rb_prev(node: *mut RbNode) -> *mut RbNode {
    if node.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        if !(*node).left.is_null() {
            let mut prev = (*node).left;
            while !(*prev).right.is_null() {
                prev = (*prev).right;
            }
            return prev;
        }
        let mut current = node;
        let mut p = parent(current);
        while !p.is_null() && core::ptr::eq((*p).left, current) {
            current = p;
            p = parent(p);
        }
        p
    }
}

/// Linux `rb_insert_color_cached()`: rebalance after `rb_link_node()`, with
/// the cached leftmost pointer updated when the new node is leftmost.
///
/// # Safety
/// `node` must be linked into `root`.
pub unsafe fn rb_insert_color_cached(node: *mut RbNode, root: *mut RbRootCached, leftmost: bool) {
    unsafe {
        if leftmost {
            (*root).leftmost = node;
        }
        insert_color(node, core::ptr::addr_of_mut!((*root).root), core::ptr::null());
    }
}
