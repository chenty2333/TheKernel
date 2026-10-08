// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Linux 7.2.3 list primitives used by the upstream i915 translations. The
// node layout is supplied by intel_engine_types.h; links remain intrusive.

use core::mem::MaybeUninit;

use crate::intel_engine_cs_upstream::ListHead;

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

pub unsafe fn list_add_tail(new: *mut ListHead, head: *mut ListHead) {
    __list_add(new, (*head).prev, head);
}

unsafe fn __list_del(prev: *mut ListHead, next: *mut ListHead) {
    (*next).prev = prev;
    (*prev).next = next;
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
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
        container_of!((*head).next, $container, $($member)+)
    }};
}

macro_rules! list_first_entry_or_null {
    ($head:expr, $container:ty, $($member:tt)+) => {{
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
        if core::ptr::eq((*head).next, head) { core::ptr::null_mut() }
        else { container_of!((*head).next, $container, $($member)+) }
    }};
}

macro_rules! list_next_entry {
    ($entry:expr, $($member:tt)+) => {{
        let entry = $entry;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).$($member)+) } as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).next)) };
        next.wrapping_sub(offset) as *mut _
    }};
}

macro_rules! list_prev_entry {
    ($entry:expr, $($member:tt)+) => {{
        let entry = $entry;
        let member = unsafe { core::ptr::addr_of_mut!((*entry).$($member)+) } as *mut $crate::intel_engine_cs_upstream::ListHead;
        let offset = (member as usize).wrapping_sub(entry as usize);
        let prev = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*member).prev)) };
        prev.wrapping_sub(offset) as *mut _
    }};
}

macro_rules! list_for_each_entry {
    ($pos:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
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
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
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

macro_rules! list_for_each_entry_reverse {
    ($pos:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
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
    ($pos:ident, $tmp:ident, $head:expr, $member:ident $(.$member_rest:ident)*, $body:block) => {{
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
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
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
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
        let head = $head as *mut $crate::intel_engine_cs_upstream::ListHead;
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
        let head = unsafe {
            core::ptr::addr_of_mut!((*$parent).parallel.children.child_list)
                as *mut $crate::intel_engine_cs_upstream::ListHead
        };
        list_for_each_entry_safe!($child, $next, head, parallel.children.child_link, $body);
    }};
}

macro_rules! for_each_child {
    ($parent:ident, $child:ident, $($body:tt)+) => {{
        let head = unsafe {
            core::ptr::addr_of_mut!((*$parent).parallel.children.child_list)
                as *mut $crate::intel_engine_cs_upstream::ListHead
        };
        list_for_each_entry!($child, head, parallel.children.child_link, { $($body)+ });
    }};
}
