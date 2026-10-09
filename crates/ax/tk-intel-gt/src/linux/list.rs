// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Linux 7.2.3 list primitives used by the upstream i915 translations. The
// node layout is supplied by intel_engine_types.h; links remain intrusive.

use core::{
    mem::MaybeUninit,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::intel_engine_cs_upstream::{ListHead, LlistHead};

/// Initialize Linux's lock-free list head.
#[inline]
pub unsafe fn init_llist_head(head: *mut LlistHead) {
    unsafe { AtomicPtr::from_ptr(core::ptr::addr_of_mut!((*head).first)) }
        .store(core::ptr::null_mut(), Ordering::Relaxed);
}

/// Push one node onto a Linux llist. Returns true when the list was empty.
#[inline]
pub unsafe fn llist_add(
    node: *mut crate::intel_engine_cs_upstream::LlistNode,
    head: *mut LlistHead,
) -> bool {
    let first = unsafe { AtomicPtr::from_ptr(core::ptr::addr_of_mut!((*head).first)) };
    let mut old = first.load(Ordering::Relaxed);
    loop {
        unsafe { (*node).next = old };
        match first.compare_exchange_weak(old, node, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return old.is_null(),
            Err(actual) => old = actual,
        }
    }
}

/// Push a linked batch onto a Linux llist. Returns true when the list was empty.
#[inline]
pub unsafe fn llist_add_batch(
    first_node: *mut crate::intel_engine_cs_upstream::LlistNode,
    last_node: *mut crate::intel_engine_cs_upstream::LlistNode,
    head: *mut LlistHead,
) -> bool {
    let first = unsafe { AtomicPtr::from_ptr(core::ptr::addr_of_mut!((*head).first)) };
    let mut old = first.load(Ordering::Relaxed);
    loop {
        unsafe { (*last_node).next = old };
        match first.compare_exchange_weak(old, first_node, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return old.is_null(),
            Err(actual) => old = actual,
        }
    }
}

/// Atomically detach every node from an llist.
#[inline]
pub unsafe fn llist_del_all(
    head: *mut LlistHead,
) -> *mut crate::intel_engine_cs_upstream::LlistNode {
    unsafe { AtomicPtr::from_ptr(core::ptr::addr_of_mut!((*head).first)) }
        .swap(core::ptr::null_mut(), Ordering::Acquire)
}

#[allow(non_snake_case)]
pub unsafe fn INIT_LIST_HEAD(head: *mut ListHead) {
    (*head).next = head;
    (*head).prev = head;
}

pub fn list_empty(head: &ListHead) -> bool {
    core::ptr::eq(head.next, head as *const ListHead as *mut ListHead)
}

pub fn list_is_singular(head: &ListHead) -> bool {
    !list_empty(head) && core::ptr::eq(head.next, head.prev)
}

unsafe fn __list_add(new: *mut ListHead, prev: *mut ListHead, next: *mut ListHead) {
    (*next).prev = new;
    (*new).next = next;
    (*new).prev = prev;
    (*prev).next = new;
}

pub unsafe fn list_add(new: *mut ListHead, head: *mut ListHead) {
    __list_add(new, head, (*head).next);
}

/// C ABI provider for the LinuxKPI `list_sort()` contract.
///
/// The generic Linux implementation is GPL-only and is not copied here. This
/// independent, allocation-free stable insertion sort has the same ordering
/// contract; the i915 engine-registration list is small and sorted once during
/// device setup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn list_sort(
    priv_: *mut core::ffi::c_void,
    head: *mut ListHead,
    cmp: unsafe extern "C" fn(
        *mut core::ffi::c_void,
        *const ListHead,
        *const ListHead,
    ) -> i32,
) {
    let mut node = unsafe { (*head).next };
    unsafe {
        (*head).next = head;
        (*head).prev = head;
    }

    while !core::ptr::eq(node, head) {
        let next = unsafe { (*node).next };
        let mut cursor = unsafe { (*head).next };
        while !core::ptr::eq(cursor, head) && unsafe { cmp(priv_, cursor, node) <= 0 } {
            cursor = unsafe { (*cursor).next };
        }

        unsafe {
            (*node).prev = (*cursor).prev;
            (*node).next = cursor;
            (*(*cursor).prev).next = node;
            (*cursor).prev = node;
        }
        node = next;
    }
}

pub unsafe fn list_add_tail(new: *mut ListHead, head: *mut ListHead) {
    __list_add(new, (*head).prev, head);
}

unsafe fn __list_del(prev: *mut ListHead, next: *mut ListHead) {
    (*next).prev = prev;
    (*prev).next = next;
}

#[inline]
pub unsafe fn __list_del_entry(entry: *mut ListHead) {
    unsafe { __list_del((*entry).prev, (*entry).next) };
}

pub unsafe fn list_del(entry: *mut ListHead) {
    __list_del((*entry).prev, (*entry).next);
    (*entry).next = 0xdead_0000_0000_0100usize as *mut ListHead;
    (*entry).prev = 0xdead_0000_0000_0122usize as *mut ListHead;
}

pub unsafe fn list_del_init(entry: *mut ListHead) {
    __list_del((*entry).prev, (*entry).next);
    INIT_LIST_HEAD(entry);
}

pub unsafe fn list_move(entry: *mut ListHead, head: *mut ListHead) {
    __list_del((*entry).prev, (*entry).next);
    list_add(entry, head);
}

pub unsafe fn list_move_tail(entry: *mut ListHead, head: *mut ListHead) {
    __list_del((*entry).prev, (*entry).next);
    list_add_tail(entry, head);
}

/// Move all entries from `list` to the beginning of `head`, then reinitialize
/// `list`, matching Linux list_splice_init().
pub unsafe fn list_splice_init(list: *mut ListHead, head: *mut ListHead) {
    if unsafe { list_empty(&*list) } {
        return;
    }
    let first = unsafe { (*list).next };
    let last = unsafe { (*list).prev };
    let at = unsafe { (*head).next };
    unsafe {
        (*first).prev = head;
        (*head).next = first;
        (*last).next = at;
        (*at).prev = last;
        INIT_LIST_HEAD(list);
    }
}

/// Insert all entries from `list` after `head` without reinitializing `list`,
/// matching Linux `list_splice()` (`include/linux/list.h`).
#[inline]
pub unsafe fn list_splice(list: *mut ListHead, head: *mut ListHead) {
    if unsafe { list_empty(&*list) } {
        return;
    }
    let first = unsafe { (*list).next };
    let last = unsafe { (*list).prev };
    let at = unsafe { (*head).next };
    unsafe {
        (*first).prev = head;
        (*head).next = first;
        (*last).next = at;
        (*at).prev = last;
    }
}

/// Insert all entries from `list` immediately before `head`, matching Linux
/// `list_splice_tail()` without reinitializing the source list.
#[inline]
pub unsafe fn list_splice_tail(list: *mut ListHead, head: *mut ListHead) {
    if unsafe { list_empty(&*list) } {
        return;
    }
    let first = unsafe { (*list).next };
    let last = unsafe { (*list).prev };
    let prev = unsafe { (*head).prev };
    unsafe {
        (*prev).next = first;
        (*first).prev = prev;
        (*last).next = head;
        (*head).prev = last;
    }
}

pub unsafe fn list_replace(old: *mut ListHead, new: *mut ListHead) {
    (*new).next = (*old).next;
    (*(*new).next).prev = new;
    (*new).prev = (*old).prev;
    (*(*new).prev).next = new;
}

pub unsafe fn list_add_rcu(new: *mut ListHead, head: *mut ListHead) {
    let next = (*head).next;
    (*new).next = next;
    (*new).prev = head;
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    (*head).next = new;
    (*next).prev = new;
}

/// Add an entry to the tail of an RCU-visible list, preserving the kernel's
/// publication order for readers that traverse from the head.
#[inline]
pub unsafe fn list_add_tail_rcu(new: *mut ListHead, head: *mut ListHead) {
    let prev = (*head).prev;
    (*new).next = head;
    (*new).prev = prev;
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    (*prev).next = new;
    (*head).prev = new;
}

pub unsafe fn list_del_rcu(entry: *mut ListHead) {
    let next = (*entry).next;
    let prev = (*entry).prev;
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
    (*prev).next = next;
    (*next).prev = prev;
    (*entry).prev = 0xdead_0000_0000_0122usize as *mut ListHead;
}

pub unsafe fn list_is_first(entry: *const ListHead, head: *const ListHead) -> bool {
    core::ptr::eq((*entry).prev, head as *mut ListHead)
}

pub unsafe fn list_is_last(entry: *const ListHead, head: *const ListHead) -> bool {
    core::ptr::eq((*entry).next, head as *mut ListHead)
}

pub unsafe fn list_is_last_rcu(entry: *const ListHead, head: *const ListHead) -> bool {
    core::ptr::eq(
        core::ptr::read_volatile(core::ptr::addr_of!((*entry).next)) as *const ListHead,
        head,
    )
}

pub unsafe fn list_count_nodes(head: *const ListHead) -> u32 {
    let mut count = 0;
    let mut node = core::ptr::read_volatile(core::ptr::addr_of!((*head).next));
    while !core::ptr::eq(node, head as *mut ListHead) {
        count += 1;
        node = core::ptr::read_volatile(core::ptr::addr_of!((*node).next));
    }
    count
}

/// `llist_empty()` from include/linux/llist.h; this is a single-copy head
/// pointer sample and, like Linux, is only a momentary observation.
pub unsafe fn llist_empty(head: *const LlistHead) -> bool {
    assert!(!head.is_null());
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*head).first)) }.is_null()
}

/// Compute a field offset for an inferred pointer type without reading the
/// uninitialized pointer slot. The temporary is valid MaybeUninit storage.
pub(crate) unsafe fn offset_from_slot<T, U>(
    slot: *mut *mut T,
    field: impl FnOnce(*const T) -> *const U,
) -> usize {
    let _ = slot;
    let storage = MaybeUninit::<T>::uninit();
    let base = storage.as_ptr();
    (field(base) as usize).wrapping_sub(base as usize)
}

macro_rules! INIT_LIST_HEAD {
    ($head:expr) => {{
        unsafe {
            $crate::linux_list::INIT_LIST_HEAD(
                $head as *mut $crate::intel_engine_cs_upstream::ListHead,
            )
        }
    }};
}

macro_rules! list_entry {
    ($pointer:expr, $container:ty, $($member:tt)+) => {{
        container_of!($pointer, $container, $($member)+)
    }};
}

macro_rules! llist_entry {
    ($pointer:expr, $container:ty, $member:ident $(.$member_rest:ident)*) => {{
        container_of!($pointer, $container, $member $(.$member_rest)*)
    }};
}

macro_rules! list_first_entry {
    ($head:expr, $container:ty, $($member:tt)+) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        unsafe { container_of!((*head).next, $container, $($member)+) }
    }};
}

macro_rules! list_last_entry {
    ($head:expr, $container:ty, $($member:tt)+) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        unsafe { container_of!((*head).prev, $container, $($member)+) }
    }};
}

macro_rules! list_first_entry_or_null {
    ($head:expr, $container:ty, $($member:tt)+) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        if unsafe { core::ptr::eq((*head).next, head) } { core::ptr::null_mut() }
        else { unsafe { container_of!((*head).next, $container, $($member)+) } }
    }};
}

macro_rules! list_next_entry {
    ($entry:expr, signal_link) => {{
        let entry = ($entry) as *mut $crate::i915_request_types_upstream::I915Request;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).signal_link) } as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
        next.wrapping_sub(offset) as *mut $crate::i915_request_types_upstream::I915Request
    }};
    ($entry:expr, parallel.children.child_link) => {{
        let entry = ($entry) as *mut $crate::intel_context_types_upstream::IntelContext;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).parallel.children.child_link) } as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
        unsafe { &mut *(next.wrapping_sub(offset) as *mut $crate::intel_context_types_upstream::IntelContext) }
    }};
    ($entry:expr, sched.link) => {{
        let entry = ($entry) as *mut $crate::i915_request_types_upstream::I915Request;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).sched.link) } as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
        next.wrapping_sub(offset) as *mut $crate::i915_request_types_upstream::I915Request
    }};
}

macro_rules! list_prev_entry {
    ($entry:expr, signal_link) => {{
        let entry = ($entry) as *mut $crate::i915_request_types_upstream::I915Request;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).signal_link) } as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let prev = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).prev)) };
        prev.wrapping_sub(offset) as *mut $crate::i915_request_types_upstream::I915Request
    }};
}

macro_rules! list_for_each_entry {
    ($pos:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = unsafe {
            $crate::linux_list::offset_from_slot(
                core::ptr::addr_of_mut!($pos),
                |base| core::ptr::addr_of!((*base).$member $(.$member_rest)*),
            )
        };
        let mut cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*head).next)) };
        while !core::ptr::eq(cursor, head) {
            let current = cursor;
            cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cursor).next)) };
            $pos = current.wrapping_sub(offset) as *mut _;
            $body
        }
    }};
}

macro_rules! list_for_each_entry_safe {
    ($pos:ident, $tmp:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = unsafe {
            $crate::linux_list::offset_from_slot(
                core::ptr::addr_of_mut!($pos),
                |base| core::ptr::addr_of!((*base).$member $(.$member_rest)*),
            )
        };
        let mut cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*head).next)) };
        while !core::ptr::eq(cursor, head) {
            let current = cursor;
            let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cursor).next)) };
            cursor = next;
            $pos = current.wrapping_sub(offset) as *mut _;
            $tmp = next.wrapping_sub(offset) as *mut _;
            $body
        }
    }};
}

// i915_scheduler.h adapts list traversal to the request link embedded in a
// priolist. The safe form snapshots the next list node before the caller may
// remove the current request, matching list_for_each_entry_safe().
macro_rules! priolist_for_each_request_consume {
    ($request:ident, $next:ident, $plist:expr, $body:block) => {{
        let mut $request: *mut $crate::i915_request_types_upstream::I915Request = core::ptr::null_mut();
        let mut $next: *mut $crate::i915_request_types_upstream::I915Request = core::ptr::null_mut();
        let __priolist = ($plist) as *const _ as *mut $crate::i915_scheduler_types_upstream::I915Priolist;
        list_for_each_entry_safe!(
            $request,
            $next,
            unsafe { core::ptr::addr_of_mut!((*__priolist).requests) },
            sched.link,
            $body
        );
    }};
}

macro_rules! priolist_for_each_request {
    ($request:ident, $plist:expr, $body:block) => {{
        let mut $request: *mut $crate::i915_request_types_upstream::I915Request = core::ptr::null_mut();
        let __priolist = ($plist) as *const _ as *mut $crate::i915_scheduler_types_upstream::I915Priolist;
        list_for_each_entry!(
            $request,
            unsafe { core::ptr::addr_of_mut!((*__priolist).requests) },
            sched.link,
            $body
        );
    }};
}

macro_rules! list_for_each_entry_reverse {
    ($pos:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = unsafe {
            $crate::linux_list::offset_from_slot(
                core::ptr::addr_of_mut!($pos),
                |base| core::ptr::addr_of!((*base).$member $(.$member_rest)*),
            )
        };
        let mut cursor: *mut $crate::intel_engine_cs_upstream::ListHead = unsafe {
            core::ptr::read_volatile(core::ptr::addr_of!((*head).prev))
        };
        while !core::ptr::eq(cursor, head) {
            let current = cursor;
            cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cursor).prev)) };
            $pos = current.wrapping_sub(offset) as *mut _;
            $body
        }
    }};
}

macro_rules! list_for_each_entry_safe_reverse {
    ($pos:ident, $tmp:ident, $container:ty, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        // Linux's C list macro declares the iteration objects at the callsite;
        // the Rust source-order form has no `let` syntax at those sites.
        let mut $pos: *mut $container = core::ptr::null_mut();
        let mut $tmp: *mut $container = core::ptr::null_mut();
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = unsafe {
            $crate::linux_list::offset_from_slot(
                core::ptr::addr_of_mut!($pos),
                |base| core::ptr::addr_of!((*base).$member $(.$member_rest)*),
            )
        };
        let mut cursor: *mut $crate::intel_engine_cs_upstream::ListHead = unsafe {
            core::ptr::read_volatile(core::ptr::addr_of!((*head).prev))
        };
        while !core::ptr::eq(cursor, head) {
            let current = cursor;
            let prev = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cursor).prev)) };
            cursor = prev;
            $pos = current.wrapping_sub(offset) as *mut _;
            $tmp = prev.wrapping_sub(offset) as *mut _;
            $body
        }
    }};
}

macro_rules! list_for_each_entry_from_reverse {
    ($pos:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = unsafe {
            $crate::linux_list::offset_from_slot(
                core::ptr::addr_of_mut!($pos),
                |base| core::ptr::addr_of!((*base).$member $(.$member_rest)*),
            )
        };
        let mut cursor = unsafe { core::ptr::addr_of_mut!((*$pos).$member $(.$member_rest)*) } as *mut $crate::intel_engine_cs_upstream::ListHead;
        while !core::ptr::eq(cursor, head) {
            let current = cursor;
            cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cursor).prev)) };
            $pos = current.wrapping_sub(offset) as *mut _;
            $body
        }
    }};
}

macro_rules! list_for_each_entry_rcu {
    ($pos:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {
        list_for_each_entry!($pos, $head, $member $(.$member_rest)*, $body)
    };
}

macro_rules! list_for_each_prev {
    ($pos:ident, $head:expr, $body:block) => {{
        let head = ($head) as *const _ as *mut $crate::intel_engine_cs_upstream::ListHead;
        let mut cursor: *mut $crate::intel_engine_cs_upstream::ListHead =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*head).prev)) };
        while !core::ptr::eq(cursor, head) {
            $pos = cursor;
            cursor = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cursor).prev)) };
            $body
        }
    }};
}

macro_rules! llist_for_each_safe {
    ($pos:ident, $tmp:ident, $head:ident, $body:block) => {{
        let mut cursor = $head;
        while !cursor.is_null() {
            let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*cursor).next)) };
            $pos = cursor;
            $tmp = next;
            cursor = next;
            $body
        }
    }};
}

macro_rules! for_each_child_safe {
    ($parent:ident, $child:ident, $next:ident, $body:block) => {{
        let mut $child: *mut $crate::intel_context_types_upstream::IntelContext = core::ptr::null_mut();
        let mut $next: *mut $crate::intel_context_types_upstream::IntelContext = core::ptr::null_mut();
        let $parent = ($parent) as *const _ as *mut $crate::intel_context_types_upstream::IntelContext;
        let head = unsafe {
            core::ptr::addr_of_mut!((*$parent).parallel.children.child_list)
                as *mut $crate::intel_engine_cs_upstream::ListHead
        };
        list_for_each_entry_safe!($child, $next, head, parallel.children.child_link, $body);
    }};
}

macro_rules! for_each_child {
    ($parent:ident, $child:ident, $($body:tt)+) => {{
        let mut $child: *mut $crate::intel_context_types_upstream::IntelContext = core::ptr::null_mut();
        let $parent = ($parent) as *const _ as *mut $crate::intel_context_types_upstream::IntelContext;
        let head = unsafe {
            core::ptr::addr_of!((*$parent).parallel.children.child_list)
                as *mut $crate::intel_engine_cs_upstream::ListHead
        };
        list_for_each_entry!($child, head, parallel.children.child_link, { $($body)+ });
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct Entry {
        link: ListHead,
        key: u32,
        serial: u32,
    }

    unsafe extern "C" fn by_key(
        _priv: *mut core::ffi::c_void,
        a: *const ListHead,
        b: *const ListHead,
    ) -> i32 {
        let a = a.cast::<Entry>();
        let b = b.cast::<Entry>();
        unsafe { (*a).key.cmp(&(*b).key) as i32 }
    }

    #[test]
    fn list_sort_is_stable_and_restores_both_links() {
        let mut head = ListHead {
            next: core::ptr::null_mut(),
            prev: core::ptr::null_mut(),
        };
        unsafe { INIT_LIST_HEAD(&mut head) };
        let mut entries = [
            Entry { link: ListHead { next: core::ptr::null_mut(), prev: core::ptr::null_mut() }, key: 2, serial: 0 },
            Entry { link: ListHead { next: core::ptr::null_mut(), prev: core::ptr::null_mut() }, key: 1, serial: 1 },
            Entry { link: ListHead { next: core::ptr::null_mut(), prev: core::ptr::null_mut() }, key: 2, serial: 2 },
        ];
        for entry in &mut entries {
            unsafe { list_add_tail(core::ptr::addr_of_mut!(entry.link), &mut head) };
        }

        unsafe { list_sort(core::ptr::null_mut(), &mut head, by_key) };

        let mut got = [(0u32, 0u32); 3];
        let mut link = head.next;
        for item in &mut got {
            assert!(!core::ptr::eq(link, &mut head));
            let entry = link.cast::<Entry>();
            *item = unsafe { ((*entry).key, (*entry).serial) };
            assert!(core::ptr::eq(unsafe { (*(*link).next).prev }, link));
            link = unsafe { (*link).next };
        }
        assert!(core::ptr::eq(link, &mut head));
        assert_eq!(got, [(1, 1), (2, 0), (2, 2)]);
    }
}
