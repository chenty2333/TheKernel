use alloc::{sync::Arc, vec::Vec};
use core::time::Duration;

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_MONOTONIC, CLOCK_REALTIME, FUTEX_WAITV_MAX, futex_waitv,
    robust_list_head, timespec,
};
use tk_linux_futex::{
    FUTEX_NO_NODE, FUTEX2_PRIVATE, Futex2Flags, FutexCommand, FutexWaitV, LegacyOp, PiAcquire,
    PiUnlock, PiWord, WakeOp, parse_futex2_flags, pi_handoff_value, plan_pi_acquire,
    plan_pi_unlock, plan_requeue, validate_numa_node, validate_requeue_flags, wake_op_count,
};

use crate::{
    mm::{
        AddrSpace, FutexMappingNamespace, SharedFutexKey, UserMemoryCapability,
        check_user_readable_with, check_user_writable_with, map_usercopy_error,
    },
    task::{
        AlarmClock, AsThread, FutexHandle, FutexKey, FutexWaitRestart, PiUnlockOutcome, PiWaiter,
        PtraceAccessMode, RestartBlock, WaitConditionError, WaitConditionResult,
        check_current_thread_ptrace_image_access, futex_table_for, get_visible_task,
        pi_boost_owner, pi_deboost_owner, wait_on_any_futex_if_atomic,
    },
    time::TimeValueLike,
};

#[derive(Clone, Copy)]
struct FutexWaitDeadline {
    clock: AlarmClock,
    deadline: Duration,
}

/// Flag grammar of the `futex2` family (`FUTEX2_VALID_MASK`, size, NUMA and
/// MPOL bits).  Queueing only supports native 32-bit words, so a size other
/// than `FUTEX2_SIZE_U32` is rejected here with `EINVAL`, exactly like
/// `futex_flags_valid()`/`futex_size()`.
fn futex2_core_flags(flags: u32) -> AxResult<Futex2Flags> {
    parse_futex2_flags(flags).map_err(|_| AxError::InvalidInput)
}

fn validate_futex2_flags(flags: u32) -> AxResult<Futex2Flags> {
    futex2_core_flags(flags)
}

fn validate_futex2_value(value: u64) -> AxResult<u32> {
    u32::try_from(value).map_err(|_| AxError::InvalidInput)
}

fn assert_unsigned(value: u32) -> AxResult<u32> {
    if (value as i32) < 0 {
        Err(AxError::InvalidInput)
    } else {
        Ok(value)
    }
}

/// Legacy `FUTEX_WAKE`/`FUTEX_WAKE_BITSET`/`FUTEX_WAKE_OP` wake limit.
///
/// `futex_wake()` loops `while (ret < nr_wake)` and wakes the first matching
/// waiter *before* testing the limit, so a zero or negative `val` still wakes
/// exactly one waiter and reports one wakeup.  Only `FUTEX_WAKE_OP`'s second
/// limit, which is applied as a plain `nr_wake2` bound, differs.
fn legacy_wake_count(value: u32) -> usize {
    let signed = value as i32;
    if signed <= 0 { 1 } else { signed as usize }
}

fn futex_key_from(
    address: usize,
    private: bool,
    aspace: &Arc<Mutex<AddrSpace>>,
) -> (FutexKey, Option<FutexMappingNamespace>) {
    if private {
        // Explicit FUTEX_PRIVATE operations intentionally skip mapping
        // namespace validation, even if the VMA is shared.
        (FutexKey::new_private(address), None)
    } else {
        let aspace = aspace.lock();
        let namespace = crate::mm::futex_mapping_namespace_at(&aspace, address);
        (FutexKey::new(&aspace, address), Some(namespace))
    }
}

/// `get_futex_key()`'s alignment rule: `address % size` must be zero, where
/// `size` is the futex width (`futex_size()`), i.e. eight bytes for a
/// `FUTEX2_NUMA` word.
fn validate_futex_address(address: *const u32, bytes: usize) -> AxResult<()> {
    if address.addr() % bytes == 0 {
        Ok(())
    } else {
        Err(AxError::InvalidInput)
    }
}

/// `access_ok(uaddr, size)`: the whole futex window must lie inside the task's
/// user address range. No page is faulted.
fn validate_futex_user_range(address: *const u32, bytes: usize) -> AxResult<()> {
    validate_futex_address(address, bytes)?;
    crate::mm::check_access(address.addr(), bytes)
}

fn validate_futex_word_read(
    address: *const u32,
    bytes: usize,
    caller: &UserMemoryCapability,
) -> AxResult<()> {
    validate_futex_address(address, bytes)?;
    check_user_readable_with(caller, address.addr(), bytes)?;
    Ok(())
}

fn validate_futex_key_access(
    address: *const u32,
    flags: Futex2Flags,
    caller: &UserMemoryCapability,
) -> AxResult<()> {
    let bytes = flags.word_bytes();
    if flags.private {
        // A private futex key is the process/address pair. Linux only applies
        // access_ok() here: FUTEX_WAKE on an in-range unmapped or PROT_NONE
        // address can therefore report zero waiters without faulting the
        // page. Shared keys still have to resolve their backing mapping.
        validate_futex_user_range(address, bytes)
    } else {
        validate_futex_word_read(address, bytes, caller)
    }
}

/// Requeue never dereferences a process-private target.  Its key is exactly
/// `(mm, address)`, so Linux requires alignment but deliberately permits low
/// or currently unmapped addresses.  A shared target still has to resolve the
/// backing mapping from the user word.
fn validate_futex_requeue_target(
    address: *const u32,
    flags: Futex2Flags,
    caller: &UserMemoryCapability,
) -> AxResult<()> {
    let bytes = flags.word_bytes();
    if flags.private {
        validate_futex_address(address, bytes)
    } else {
        validate_futex_word_read(address, bytes, caller)
    }
}

/// `get_futex_key()`'s `FUTEX2_NUMA` protocol (`kernel/futex/core.c`).
///
/// A `FUTEX2_NUMA` futex is eight bytes: the first word is the futex itself and
/// the second carries the NUMA node the key must be hashed into. Linux reads
/// that word through `get_user_inline()`, rejects anything that is neither
/// `FUTEX_NO_NODE` nor a possible node with `EINVAL`, and replaces
/// `FUTEX_NO_NODE` with `numa_node_id()` so userspace can publish the placement
/// it observed.
///
/// This kernel has a single node, so `numa_node_id()`, `first_node()` and
/// `home_node()` all resolve to node 0 and the placement is fully determined by
/// the same rewrite; the observable contract — accept `FUTEX_NO_NODE`, accept
/// node 0, reject any other node, and write the resolved node back into an
/// eight-byte word — is implemented exactly. A read-only or unmapped node word
/// faults with `EFAULT`, as `get_user_inline()`/`put_user_inline()` do.
fn futex_numa_node(
    caller: &UserMemoryCapability,
    address: usize,
    flags: Futex2Flags,
) -> AxResult<()> {
    if !flags.numa {
        return Ok(());
    }
    let node_address = address
        .checked_add(size_of::<u32>())
        .ok_or(AxError::BadAddress)?;
    let node = fault_read_u32(caller, node_address)?;
    let resolved = validate_numa_node(node).map_err(|_| AxError::InvalidInput)?;
    if node == FUTEX_NO_NODE {
        check_user_writable_with(caller, node_address, size_of::<u32>())?;
        caller
            .write_value(node_address as *mut u32, resolved)
            .map_err(map_usercopy_error)?;
    }
    Ok(())
}

/// A non-PRIVATE futex key carries a mapping namespace that must be checked
/// again at the queue linearization point. This includes an address-based key
/// resolved from a private/COW VMA: it must not be confused with an explicit
/// PRIVATE operation. An explicit process-private key is only the current
/// address-space/address pair, so Linux deliberately does not fault or inspect
/// its target PTE during requeue.
fn requeue_mapping_check(private: bool, expected_namespace: Option<FutexMappingNamespace>) -> bool {
    !private && expected_namespace.is_some()
}

fn nofault_u32_read(
    address: usize,
    aspace: &Arc<Mutex<AddrSpace>>,
    expected_namespace: Option<FutexMappingNamespace>,
    expected: Option<&SharedFutexKey>,
) -> WaitConditionResult<u32> {
    let Some(aspace) = aspace.try_lock() else {
        return Err(WaitConditionError::Retry);
    };
    match crate::mm::try_read_user_u32_nofault_locked(
        &aspace,
        address,
        expected_namespace,
        expected,
    ) {
        Ok(value) => Ok(value),
        Err(crate::mm::UserU32NofaultError::Retry) => Err(WaitConditionError::Retry),
        Err(crate::mm::UserU32NofaultError::BadAddress) => {
            Err(WaitConditionError::Fault(AxError::BadAddress))
        }
    }
}

fn nofault_read_and_validate_pair(
    source: usize,
    source_namespace: Option<FutexMappingNamespace>,
    source_expected: Option<&SharedFutexKey>,
    target: usize,
    target_namespace: Option<FutexMappingNamespace>,
    target_expected: Option<&SharedFutexKey>,
    target_private: bool,
    expected: u32,
    aspace: &Arc<Mutex<AddrSpace>>,
) -> WaitConditionResult<bool> {
    let Some(aspace) = aspace.try_lock() else {
        return Err(WaitConditionError::Retry);
    };
    if requeue_mapping_check(target_private, target_namespace) {
        crate::mm::try_validate_futex_mapping_nofault_locked(
            &aspace,
            target,
            target_namespace,
            target_expected,
        )
        .map_err(|error| match error {
            crate::mm::UserU32NofaultError::Retry => WaitConditionError::Retry,
            crate::mm::UserU32NofaultError::BadAddress => {
                WaitConditionError::Fault(AxError::BadAddress)
            }
        })?;
    }
    crate::mm::try_read_user_u32_nofault_locked(&aspace, source, source_namespace, source_expected)
        .map(|value| value == expected)
        .map_err(|error| match error {
            crate::mm::UserU32NofaultError::Retry => WaitConditionError::Retry,
            crate::mm::UserU32NofaultError::BadAddress => {
                WaitConditionError::Fault(AxError::BadAddress)
            }
        })
}

fn nofault_validate_pair(
    source: usize,
    source_namespace: Option<FutexMappingNamespace>,
    source_expected: Option<&SharedFutexKey>,
    source_private: bool,
    target: usize,
    target_namespace: Option<FutexMappingNamespace>,
    target_expected: Option<&SharedFutexKey>,
    target_private: bool,
    aspace: &Arc<Mutex<AddrSpace>>,
) -> WaitConditionResult<bool> {
    let Some(aspace) = aspace.try_lock() else {
        return Err(WaitConditionError::Retry);
    };
    if requeue_mapping_check(source_private, source_namespace) {
        crate::mm::try_validate_futex_mapping_nofault_locked(
            &aspace,
            source,
            source_namespace,
            source_expected,
        )
        .map_err(|error| match error {
            crate::mm::UserU32NofaultError::Retry => WaitConditionError::Retry,
            crate::mm::UserU32NofaultError::BadAddress => {
                WaitConditionError::Fault(AxError::BadAddress)
            }
        })?;
    }
    if requeue_mapping_check(target_private, target_namespace) {
        crate::mm::try_validate_futex_mapping_nofault_locked(
            &aspace,
            target,
            target_namespace,
            target_expected,
        )
        .map_err(|error| match error {
            crate::mm::UserU32NofaultError::Retry => WaitConditionError::Retry,
            crate::mm::UserU32NofaultError::BadAddress => {
                WaitConditionError::Fault(AxError::BadAddress)
            }
        })?;
    }
    Ok(true)
}

/// Wakes one futex after holding the resolved mapping namespace stable through
/// the table lookup and queue operation.  Explicit PRIVATE operations skip
/// this VMA check by design; non-PRIVATE operations must not publish a wake
/// through a stale private namespace.
fn wake_futex(
    address: usize,
    private: bool,
    wake_count: usize,
    bitset: u32,
    aspace: &Arc<Mutex<AddrSpace>>,
    caller: &UserMemoryCapability,
) -> AxResult<usize> {
    loop {
        let (key, expected_namespace) = futex_key_from(address, private, aspace);
        let expected_key = key.shared_key().cloned();
        if expected_namespace.is_some() {
            let aspace_guard = aspace.lock();
            match crate::mm::try_validate_futex_mapping_nofault_locked(
                &aspace_guard,
                address,
                expected_namespace,
                expected_key.as_ref(),
            ) {
                Ok(()) => {}
                Err(crate::mm::UserU32NofaultError::Retry) => {
                    drop(aspace_guard);
                    let _ = fault_read_u32(caller, address)?;
                    continue;
                }
                Err(crate::mm::UserU32NofaultError::BadAddress) => {
                    return Err(AxError::BadAddress);
                }
            }
            let futex_table = futex_table_for(&key);
            let futex = futex_table.get(&key);
            return match futex.map(|futex| futex.wq.wake_inner(wake_count, bitset, true)) {
                None | Some(Ok(0)) => Ok(0),
                Some(Ok(woke)) => Ok(woke),
                Some(Err(())) => Err(AxError::InvalidInput),
            };
        }

        let futex_table = futex_table_for(&key);
        let futex = futex_table.get(&key);
        match futex.map(|futex| futex.wq.wake_inner(wake_count, bitset, true)) {
            None | Some(Ok(0)) => return Ok(0),
            Some(Ok(woke)) => return Ok(woke),
            Some(Err(())) => return Err(AxError::InvalidInput),
        }
    }
}

fn fault_read_u32(caller: &UserMemoryCapability, address: usize) -> AxResult<u32> {
    check_user_readable_with(caller, address, size_of::<u32>())?;
    caller
        .read_value(address as *const u32)
        .map_err(map_usercopy_error)
}

fn checked_user_array_address<T>(
    base: *const T,
    count: usize,
    caller: &UserMemoryCapability,
) -> AxResult<usize> {
    if base.is_null() {
        return Err(AxError::BadAddress);
    }
    let base_addr = base.addr();
    let bytes = count
        .checked_mul(size_of::<T>())
        .ok_or(AxError::BadAddress)?;
    check_user_readable_with(caller, base_addr, bytes)?;
    Ok(base_addr)
}

fn read_checked_array_entry<T>(
    base_addr: usize,
    index: usize,
    caller: &UserMemoryCapability,
) -> AxResult<T> {
    let offset = index
        .checked_mul(size_of::<T>())
        .ok_or(AxError::BadAddress)?;
    let address = base_addr.checked_add(offset).ok_or(AxError::BadAddress)?;
    // The complete byte range was checked and faulted in before this integer
    // address was formed, so this pointer never performs unchecked OOB
    // arithmetic.
    let pointer = address as *const T;
    let value = caller
        .read_value_uninit(pointer)
        .map_err(map_usercopy_error)?;
    // SAFETY: the complete element range was checked and copied in above.
    Ok(unsafe { value.assume_init() })
}

fn futex_clock(clock: tk_linux_futex::Clock) -> AlarmClock {
    match clock {
        tk_linux_futex::Clock::Realtime => AlarmClock::Realtime,
        tk_linux_futex::Clock::Monotonic => AlarmClock::Monotonic,
    }
}

/// `futex_init_timeout()`: convert the user `timespec` of a timed opcode into
/// an absolute deadline on the opcode's clock.
///
/// `FUTEX_WAIT` is the only *relative* timeout; `FUTEX_WAIT_BITSET`,
/// `FUTEX_WAIT_REQUEUE_PI`, `FUTEX_LOCK_PI` and `FUTEX_LOCK_PI2` all take an
/// absolute one. `FUTEX_LOCK_PI` is pinned to `CLOCK_REALTIME` because
/// `do_futex()` forces `FLAGS_CLOCKRT` for it, while `FUTEX_LOCK_PI2` honours
/// the explicit `FUTEX_CLOCK_REALTIME` bit.
fn futex_wait_deadline(
    op: LegacyOp,
    timeout: *const timespec,
    caller: &UserMemoryCapability,
) -> AxResult<FutexWaitDeadline> {
    let ts = timeout;
    let ts = caller.read_value_uninit(ts).map_err(map_usercopy_error)?;
    // SAFETY: the explicit usercopy initialized the complete timespec.
    let ts = unsafe { ts.assume_init() }.try_into_time_value()?;
    let clock = futex_clock(op.timeout_clock());
    let deadline = if op.timeout_is_relative() {
        clock.now().checked_add(ts).unwrap_or(Duration::MAX)
    } else {
        ts
    };
    Ok(FutexWaitDeadline { clock, deadline })
}

fn do_futex_wait(
    aspace: Arc<Mutex<AddrSpace>>,
    caller: &UserMemoryCapability,
    uaddr: *const u32,
    value: u32,
    bitset: u32,
    timeout: Option<FutexWaitDeadline>,
    private: bool,
) -> AxResult<isize> {
    loop {
        let observed = fault_read_u32(caller, uaddr.addr())?;
        if observed != value {
            return Err(AxError::WouldBlock);
        }

        // Resolve the process image before entering the queue gate. The
        // nofault callback may only try-lock this captured address space.
        let (key, expected_namespace) = futex_key_from(uaddr.addr(), private, &aspace);
        let expected_key = key.shared_key().cloned();
        let futex_table = futex_table_for(&key);
        // Keep an owned table handle while the waiter may outlive this
        // syscall's queue-gate attempt. Its owner token can then remove an
        // idle target entry after a later requeue cancellation.
        let futex = futex_table.get_or_insert_owned(&key);
        let result = futex.wq.wait_if(
            futex.waiter_owner(),
            bitset,
            timeout.map(|it| (it.clock, it.deadline)),
            || {
                nofault_u32_read(
                    uaddr.addr(),
                    &aspace,
                    expected_namespace,
                    expected_key.as_ref(),
                )
                .map(|current| current == value)
            },
        );
        match result {
            Ok(true) => return Ok(0),
            Ok(false) => return Err(AxError::WouldBlock),
            Err(WaitConditionError::Retry) => {
                // All queue gates have been released by `wait_if`. Fault/read
                // in task context, then retry the same futex operation.
                let _ = fault_read_u32(caller, uaddr.addr())?;
            }
            Err(WaitConditionError::Fault(error)) => return Err(error),
        }
    }
}

fn validate_waitv_timeout(
    timeout: *const timespec,
    clockid: __kernel_clockid_t,
    caller: &UserMemoryCapability,
) -> AxResult<Option<FutexWaitDeadline>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let clock = match clockid as u32 {
        CLOCK_REALTIME => AlarmClock::Realtime,
        CLOCK_MONOTONIC => AlarmClock::Monotonic,
        _ => return Err(AxError::InvalidInput),
    };
    let ts = caller
        .read_value_uninit(timeout)
        .map_err(map_usercopy_error)?;
    // SAFETY: the explicit usercopy initialized the complete timespec.
    let ts = unsafe { ts.assume_init() }.try_into_time_value()?;
    Ok(Some(FutexWaitDeadline {
        clock,
        deadline: ts,
    }))
}

fn validate_waitv_entry(waiter: &futex_waitv) -> AxResult<()> {
    if waiter.__reserved != 0 {
        return Err(AxError::InvalidInput);
    }
    validate_futex2_flags(waiter.flags)?;
    validate_futex2_value(waiter.val)?;
    if waiter.uaddr == 0 {
        return Err(AxError::BadAddress);
    }
    if !(waiter.uaddr as *const u32).is_aligned() {
        return Err(AxError::InvalidInput);
    }
    Ok(())
}

const fn futex_waitv_abi(waiter: futex_waitv) -> FutexWaitV {
    FutexWaitV {
        val: waiter.val,
        uaddr: waiter.uaddr,
        flags: waiter.flags,
        reserved: waiter.__reserved,
    }
}

pub fn sys_futex_waitv(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    waiters: *const futex_waitv,
    nr_futexes: u32,
    flags: u32,
    timeout: *const timespec,
    clockid: __kernel_clockid_t,
) -> AxResult<isize> {
    debug!(
        "sys_futex_waitv <= waiters: {waiters:?}, nr_futexes: {nr_futexes}, flags: {flags}, \
         timeout: {timeout:?}, clockid: {clockid}",
    );

    let caller = UserMemoryCapability::new(caller_aspace.clone());

    if flags != 0 || nr_futexes == 0 || nr_futexes > FUTEX_WAITV_MAX || waiters.is_null() {
        return Err(AxError::InvalidInput);
    }

    let timeout = validate_waitv_timeout(timeout, clockid, &caller)?;
    let waiters_addr = checked_user_array_address(waiters, nr_futexes as usize, &caller)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(nr_futexes as usize)
        .map_err(|_| AxError::NoMemory)?;
    for index in 0..nr_futexes as usize {
        let waiter: futex_waitv = read_checked_array_entry(waiters_addr, index, &caller)?;
        validate_waitv_entry(&waiter)?;
        let address = waiter.uaddr as usize;
        // Fault in every futex word before any queue gate is acquired. This
        // also makes a no-fault Retry below an explicit task-context retry.
        let _ = fault_read_u32(&caller, address)?;
        entries.push(waiter);
    }

    loop {
        // Capture the process image before resolving shared futex keys. The
        // same address-space snapshot must back both key derivation and the
        // later no-fault comparison attempt; a Retry starts a fresh iteration
        // after task-context fault/read work.
        let aspace = caller_aspace.clone();
        let mut futexes: Vec<(FutexHandle, u32)> = Vec::new();
        let mut expected_namespaces: Vec<Option<FutexMappingNamespace>> = Vec::new();
        let mut expected_keys: Vec<Option<SharedFutexKey>> = Vec::new();
        futexes
            .try_reserve_exact(entries.len())
            .map_err(|_| AxError::NoMemory)?;
        expected_namespaces
            .try_reserve_exact(entries.len())
            .map_err(|_| AxError::NoMemory)?;
        expected_keys
            .try_reserve_exact(entries.len())
            .map_err(|_| AxError::NoMemory)?;
        for waiter in &entries {
            let private = waiter.flags & FUTEX2_PRIVATE != 0;
            let (key, expected_namespace) = futex_key_from(waiter.uaddr as usize, private, &aspace);
            let futex_table = futex_table_for(&key);
            let futex = futex_table.get_or_insert_owned(&key);
            futexes.push((futex, u32::MAX));
            expected_namespaces.push(expected_namespace);
            expected_keys.push(key.shared_key().cloned());
        }

        let result =
            wait_on_any_futex_if_atomic(futexes, timeout.map(|it| (it.clock, it.deadline)), || {
                let Some(aspace) = aspace.try_lock() else {
                    return Err(WaitConditionError::Retry);
                };
                for (index, waiter) in entries.iter().enumerate() {
                    let value = crate::mm::try_read_user_u32_nofault_locked(
                        &aspace,
                        waiter.uaddr as usize,
                        expected_namespaces[index],
                        expected_keys[index].as_ref(),
                    )
                    .map_err(|error| match error {
                        crate::mm::UserU32NofaultError::Retry => WaitConditionError::Retry,
                        crate::mm::UserU32NofaultError::BadAddress => {
                            WaitConditionError::Fault(AxError::BadAddress)
                        }
                    })?;
                    if value != waiter.val as u32 {
                        return Ok(false);
                    }
                }
                Ok(true)
            });
        match result {
            Ok(index) => return Ok(index as isize),
            Err(WaitConditionError::Retry) => {
                // `wait_on_any_futex_if_atomic` has released every queue gate and
                // cleaned up all partial registrations before returning Retry.
                for waiter in &entries {
                    let _ = fault_read_u32(&caller, waiter.uaddr as usize)?;
                }
            }
            Err(WaitConditionError::Fault(error)) => return Err(error),
        }
    }
}

/// Wake waiters on a 32-bit futex using the futex2 ABI.
///
/// Linux x86_64 assigns syscall number 454 to this operation.
/// The `nr` argument is signed in the ABI; Linux's futex core treats zero as
/// the strict no-op fast path and a negative value as a request that still
/// wakes the first matching waiter.
pub fn sys_futex_wake(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    uaddr: *const u32,
    mask: u64,
    nr: i32,
    flags: u32,
) -> AxResult<isize> {
    debug!("sys_futex_wake <= uaddr: {uaddr:?}, mask: {mask:#x}, nr: {nr}, flags: {flags:#x}",);

    let futex_flags = validate_futex2_flags(flags)?;
    let private = futex_flags.private;
    let caller = UserMemoryCapability::new(caller_aspace.clone());
    let mask = validate_futex2_value(mask)?;
    if mask == 0 {
        return Err(AxError::InvalidInput);
    }

    // Linux resolves the futex key before the strict zero-wake fast path, so
    // alignment, access and the FUTEX2_NUMA node word are all validated even
    // when no waiter can be woken.
    validate_futex_key_access(uaddr, futex_flags, &caller)?;
    futex_numa_node(&caller, uaddr.addr(), futex_flags)?;
    if nr == 0 {
        return Ok(0);
    }

    let wake_count = if nr < 0 { 1 } else { nr as usize };
    let count = wake_futex(
        uaddr.addr(),
        private,
        wake_count,
        mask,
        &caller_aspace,
        &caller,
    )?;
    Ok(count as isize)
}

/// Wait on a 32-bit futex using an optional absolute futex2 timeout.
pub fn sys_futex_wait(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    uaddr: *const u32,
    value: u64,
    mask: u64,
    flags: u32,
    timeout: *const timespec,
    clockid: __kernel_clockid_t,
) -> AxResult<isize> {
    debug!(
        "sys_futex_wait <= uaddr: {uaddr:?}, value: {value:#x}, mask: {mask:#x}, flags: \
         {flags:#x}, timeout: {timeout:?}, clockid: {clockid}",
    );

    let caller = UserMemoryCapability::new(caller_aspace.clone());
    let futex_flags = validate_futex2_flags(flags)?;
    let private = futex_flags.private;
    let value = validate_futex2_value(value)?;
    let mask = validate_futex2_value(mask)?;
    if mask == 0 {
        return Err(AxError::InvalidInput);
    }
    // `futex2_setup_timeout()` runs before `__futex_wait()`, so the clock and
    // the user timespec are validated before the futex key is resolved.
    let timeout = validate_waitv_timeout(timeout, clockid, &caller)?;
    validate_futex_word_read(uaddr, futex_flags.word_bytes(), &caller)?;
    futex_numa_node(&caller, uaddr.addr(), futex_flags)?;
    if let Some(timeout) = timeout {
        current()
            .as_thread()
            .install_restart_block(RestartBlock::FutexWait(FutexWaitRestart {
                uaddr: uaddr.addr(),
                expected: value,
                bitset: mask,
                deadline: timeout.deadline,
                clock: timeout.clock,
                private,
            }));
    }
    do_futex_wait(caller_aspace, &caller, uaddr, value, mask, timeout, private)
}

/// Requeue waiters from one futex2 address to another.
pub fn sys_futex_requeue(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    waiters: *const futex_waitv,
    flags: u32,
    nr_wake: i32,
    nr_requeue: i32,
) -> AxResult<isize> {
    debug!(
        "sys_futex_requeue <= waiters: {waiters:?}, flags: {flags:#x}, nr_wake: {nr_wake}, \
         nr_requeue: {nr_requeue}",
    );

    if validate_requeue_flags(flags).is_err() || waiters.is_null() {
        return Err(AxError::InvalidInput);
    }

    let caller = UserMemoryCapability::new(caller_aspace.clone());

    // Validate and fault in the complete two-entry byte range before forming
    // any element pointers. This removes unchecked pointer arithmetic from a
    // user-controlled descriptor array.
    let waiters_addr = checked_user_array_address(waiters, 2, &caller)?;
    // futex_requeue parses both entries before validating the signed counts.
    let source: futex_waitv = read_checked_array_entry(waiters_addr, 0, &caller)?;
    let target: futex_waitv = read_checked_array_entry(waiters_addr, 1, &caller)?;
    let plan = plan_requeue(
        futex_waitv_abi(source),
        futex_waitv_abi(target),
        flags,
        nr_wake,
        nr_requeue,
    )
    .map_err(|_| AxError::InvalidInput)?;
    let source_private = plan.source.private;
    let target_private = plan.target.private;
    let source_flags = Futex2Flags {
        private: source_private,
        numa: plan.flags.numa,
        mpol: plan.flags.mpol,
    };
    let target_flags = Futex2Flags {
        private: target_private,
        numa: plan.flags.numa,
        mpol: plan.flags.mpol,
    };
    let source_uaddr = plan.source.address as *const u32;
    let target_uaddr = plan.target.address as *const u32;
    let _ = fault_read_u32(&caller, source_uaddr.addr())?;
    futex_numa_node(&caller, source_uaddr.addr(), source_flags)?;
    validate_futex_requeue_target(target_uaddr, target_flags, &caller)?;
    futex_numa_node(&caller, target_uaddr.addr(), target_flags)?;

    loop {
        // A no-fault retry may observe a concurrent unmap/remap. Re-resolve
        // both keys after the task-context read so the next linearization
        // attempt cannot use a queue for the old mapping.
        // Resolve the process image before taking either queue gate. A
        // nofault comparison must not acquire the image RwLock in-gate.
        let aspace = caller_aspace.clone();
        let (source_key, source_namespace) =
            futex_key_from(source_uaddr.addr(), source_private, &aspace);
        let source_expected = source_key.shared_key().cloned();
        let source_table = futex_table_for(&source_key);
        let source_futex = source_table.get_or_insert_owned(&source_key);
        let (target_key, target_namespace) =
            futex_key_from(target_uaddr.addr(), target_private, &aspace);
        let target_expected = target_key.shared_key().cloned();
        let target_table = futex_table_for(&target_key);
        let target_futex = target_table.get_or_insert_owned(&target_key);
        let result = source_futex.wq.wake_and_requeue_if(
            plan.wake,
            plan.requeue,
            &target_futex.wq,
            target_futex.waiter_owner(),
            u32::MAX,
            || {
                nofault_read_and_validate_pair(
                    source_uaddr.addr(),
                    source_namespace,
                    source_expected.as_ref(),
                    target_uaddr.addr(),
                    target_namespace,
                    target_expected.as_ref(),
                    target_private,
                    plan.source.expected,
                    &aspace,
                )
            },
        );
        match result {
            Ok(Some(result)) => return Ok((result.0 + result.1) as isize),
            Ok(None) => return Err(AxError::WouldBlock),
            Err(WaitConditionError::Retry) => {
                let _ = fault_read_u32(&caller, source_uaddr.addr())?;
                if requeue_mapping_check(target_private, target_namespace) {
                    let _ = fault_read_u32(&caller, target_uaddr.addr())?;
                }
            }
            Err(WaitConditionError::Fault(error)) => return Err(error),
        }
    }
}

pub(crate) fn restart_futex_wait(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    block: FutexWaitRestart,
) -> AxResult<isize> {
    let caller = UserMemoryCapability::new(caller_aspace.clone());
    do_futex_wait(
        caller_aspace,
        &caller,
        block.uaddr as *const u32,
        block.expected,
        block.bitset,
        Some(FutexWaitDeadline {
            clock: block.clock,
            deadline: block.deadline,
        }),
        block.private,
    )
}

/// `futex_robust_unlock()`: the `FUTEX_ROBUST_UNLOCK` modifier turns a
/// `FUTEX_WAKE`/`FUTEX_WAKE_BITSET` into a store-release of zero into the futex
/// word followed by clearing the robust list's pending-op slot, and appends the
/// same slot clear to a successful `FUTEX_UNLOCK_PI`.
///
/// `pop` is the `uaddr2` argument. It is an eight-byte pointer slot, or a
/// four-byte one under `FUTEX_ROBUST_LIST32`. Either store failing is `EFAULT`.
fn futex_robust_list_clear_pending(
    caller: &UserMemoryCapability,
    pop: *mut u32,
    robust_list32: bool,
) -> AxResult<()> {
    if pop.is_null() {
        return Err(AxError::BadAddress);
    }
    let bytes = if robust_list32 {
        size_of::<u32>()
    } else {
        size_of::<u64>()
    };
    check_user_writable_with(caller, pop.addr(), bytes)?;
    let address = pop.addr();
    if robust_list32 {
        caller
            .write_value(address as *mut u32, 0u32)
            .map_err(map_usercopy_error)?;
    } else {
        caller
            .write_value(address as *mut u64, 0u64)
            .map_err(map_usercopy_error)?;
    }
    Ok(())
}

/// The `uaddr` half of `futex_robust_unlock()`: a release store of zero into
/// the futex word, before the wake or the PI unlock runs.
fn futex_robust_unlock_store(caller: &UserMemoryCapability, uaddr: *const u32) -> AxResult<()> {
    check_user_writable_with(caller, uaddr.addr(), size_of::<u32>())?;
    caller
        .write_value(uaddr.cast_mut(), 0u32)
        .map_err(map_usercopy_error)
}

// ---------------------------------------------------------------------------
// Priority inheritance.
//
// The user-word protocol below is `kernel/futex/pi.c`: `futex_lock_pi_atomic()`
// classifies the word, `attach_to_pi_state()`/`attach_to_pi_owner()` bind a
// `futex_pi_state` to the task named in it, `wake_futex_pi()` promotes the top
// waiter on unlock, and `fixup_pi_state_owner()` reconciles the word with the
// kernel state. The owner boost itself is `rt_mutex_setprio()`, which this
// kernel expresses with `axtask::set_sched_state` on the task's real RT/FIFO
// scheduling parameters (see `crate::task::futex::pi_boost`).
// ---------------------------------------------------------------------------

/// `cmpxchg(uaddr, expected, new)` on the user word, at the queue
/// linearization point.
///
/// Linux has a real user-memory `cmpxchg`; here the compare and the store are
/// carried out by one IRQ-safe atomic RMW on the resident direct mapping while
/// the address-space guard pins the page, which gives the same
/// read-compare-write atomicity against userspace and against other CPUs.
/// Returns `true` when the word still held `expected`.
fn futex_cas_at(
    address: usize,
    namespace: Option<FutexMappingNamespace>,
    key: Option<&SharedFutexKey>,
    expected: u32,
    new: u32,
    aspace: &Arc<Mutex<AddrSpace>>,
) -> WaitConditionResult<bool> {
    let Some(aspace) = aspace.try_lock() else {
        return Err(WaitConditionError::Retry);
    };
    crate::mm::try_update_user_u32_nofault_locked(&aspace, address, namespace, key, |old| {
        if old == expected { new } else { old }
    })
    .map(|previous| previous == expected)
    .map_err(|error| match error {
        crate::mm::UserU32NofaultError::Retry => WaitConditionError::Retry,
        crate::mm::UserU32NofaultError::BadAddress => {
            WaitConditionError::Fault(AxError::BadAddress)
        }
    })
}

/// `FUTEX_LOCK_PI`, `FUTEX_LOCK_PI2` and `FUTEX_TRYLOCK_PI`.
///
/// Linux v7.2.3, `kernel/futex/pi.c`:
///
/// ```c
/// int futex_lock_pi(u32 __user *uaddr, unsigned int flags, ktime_t *time, int trylock)
/// {
/// 	...
/// 		ret = futex_lock_pi_atomic(uaddr, hb, &q.key, &q.pi_state, current,
/// 					   &exiting, 0);
/// ```
///
/// `futex_lock_pi_atomic()` returns `1` when the caller took ownership without
/// blocking, `0` when it must wait, and the errnos `-EFAULT` (word not
/// readable), `-EDEADLK` (the word already names the caller), `-ESRCH`
/// (`attach_to_pi_owner()`'s `find_get_task_by_vpid()` found no owner) and
/// `-EAGAIN` (the owner is `PF_EXITING`). `trylock` maps to
/// `FUTEX_TRYLOCK_PI`, which returns `-EAGAIN` on contention instead of
/// queueing, and `deadline` is `None` for an infinite wait.
fn do_futex_lock_pi(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    caller: &UserMemoryCapability,
    uaddr: *const u32,
    private: bool,
    deadline: Option<FutexWaitDeadline>,
    trylock: bool,
) -> AxResult<isize> {
    let address = uaddr.addr();
    let self_task = current();
    let tid = self_task.as_thread().tid();
    // `futex_lock_pi()` clamps the deadline to "now" once, so the retry loop
    // below can never extend a timeout that has already elapsed.
    let deadline = deadline.map(|deadline| FutexWaitDeadline {
        deadline: deadline.deadline.max(deadline.clock.now()),
        clock: deadline.clock,
    });

    loop {
        let (key, namespace) = futex_key_from(address, private, &caller_aspace);
        let expected_key = key.shared_key().cloned();
        let table = futex_table_for(&key);
        let futex = table.get_or_insert_owned(&key);
        let pi_state = futex.pi_state();

        let observed =
            match nofault_u32_read(address, &caller_aspace, namespace, expected_key.as_ref()) {
                Ok(observed) => observed,
                Err(WaitConditionError::Retry) => {
                    let _ = fault_read_u32(caller, address)?;
                    continue;
                }
                Err(WaitConditionError::Fault(error)) => return Err(error),
            };
        let word = PiWord::decode(observed);
        if word.tid == tid {
            return Err(LinuxError::EDEADLK.into());
        }

        let mut decided: Option<AxResult<isize>> = None;
        let mut retry = false;
        let result = futex.wq.wait_pi(
            futex.waiter_owner(),
            u32::MAX,
            deadline.map(|deadline| (deadline.clock, deadline.deadline)),
            || {
                // Under the queue gate: re-read the word, then take it over or
                // publish FUTEX_WAITERS and attach to the live owner.
                let current_word =
                    nofault_u32_read(address, &caller_aspace, namespace, expected_key.as_ref())?;
                match plan_pi_acquire(PiWord::decode(current_word), tid, !trylock) {
                    PiAcquire::Deadlock => {
                        decided = Some(Err(LinuxError::EDEADLK.into()));
                        Ok(None)
                    }
                    PiAcquire::TakeOver { new_value } => {
                        if futex_cas_at(
                            address,
                            namespace,
                            expected_key.as_ref(),
                            current_word,
                            new_value,
                            &caller_aspace,
                        )? {
                            pi_state.attach(tid);
                            decided = Some(Ok(0));
                            Ok(None)
                        } else {
                            retry = true;
                            Ok(None)
                        }
                    }
                    PiAcquire::SetWaiters { new_value } => {
                        if trylock {
                            // `rt_mutex_futex_trylock()` failed: the futex is
                            // owned by a live task. Linux reports the trylock
                            // result, not -EAGAIN, as -EWOULDBLOCK, which is
                            // the same errno.
                            decided = Some(Err(AxError::WouldBlock));
                            return Ok(None);
                        }
                        if !futex_cas_at(
                            address,
                            namespace,
                            expected_key.as_ref(),
                            current_word,
                            new_value,
                            &caller_aspace,
                        )? {
                            retry = true;
                            return Ok(None);
                        }
                        pi_state.attach(PiWord::decode(current_word).tid);
                        Ok(Some(PiWaiter::new(tid, axtask::sched_state(&current()))))
                    }
                }
            },
            |waiter| {
                // `attach_to_pi_owner()`: the owner named by the user word must
                // name a live, non-exiting task. A TID that names nothing is
                // `-ESRCH`; a task that is already leaving is `-EAGAIN`, which
                // is the errno Linux surfaces when the exit-time fixup has not
                // published `FUTEX_OWNER_DIED` yet.
                let owner_tid = pi_state.owner_tid();
                match crate::task::get_visible_task_including_exiting(owner_tid) {
                    Err(_) => return Err(AxError::NoSuchProcess),
                    Ok(task) if task.as_thread().pending_exit() => {
                        return Err(AxError::WouldBlock);
                    }
                    Ok(_) => {}
                }
                pi_boost_owner(&pi_state, waiter);
                Ok(())
            },
        );

        match result {
            Ok(false) => {
                if let Some(decided) = decided {
                    return decided;
                }
                if retry {
                    let _ = fault_read_u32(caller, address)?;
                    continue;
                }
                // Unreachable: the condition always either decides or waits.
                continue;
            }
            Ok(true) => {
                // The unlock handed the futex to us (`wake_futex_pi()`), or the
                // word already names us after a lost race.
                let observed = fault_read_u32(caller, address)?;
                if PiWord::decode(observed).tid == tid {
                    pi_state.attach(tid);
                    return Ok(0);
                }
                if let Some(deadline) = deadline
                    && deadline.clock.now() >= deadline.deadline
                {
                    return Err(AxError::TimedOut);
                }
                continue;
            }
            Err(WaitConditionError::Retry) => {
                let _ = fault_read_u32(caller, address)?;
                continue;
            }
            Err(WaitConditionError::Fault(error)) => return Err(error),
        }
    }
}

/// `FUTEX_UNLOCK_PI`.
///
/// Linux v7.2.3, `kernel/futex/pi.c`:
///
/// ```c
/// int futex_unlock_pi(u32 __user *uaddr, unsigned int flags, void __user *pop)
/// {
/// 	int ret = __futex_unlock_pi(uaddr, flags);
///
/// 	if (ret || !(flags & FLAGS_ROBUST_UNLOCK))
/// 		return ret;
///
/// 	if (!futex_robust_list_clear_pending(pop, flags))
/// 		return -EFAULT;
/// ```
///
/// and, inside `__futex_unlock_pi()` for the uncontended case:
///
/// ```c
/// 	if ((ret = futex_cmpxchg_value_locked(&curval, uaddr, uval, 0))) {
/// ```
///
/// A caller that does not own the word gets `-EPERM`; with no PI waiter the
/// word becomes `0`; otherwise `wake_futex_pi()` publishes
/// `top_waiter->task->pid | FUTEX_WAITERS` *before* waking the promoted
/// waiter, so the waiter observes itself as the owner when it runs.
fn do_futex_unlock_pi(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    caller: &UserMemoryCapability,
    uaddr: *const u32,
    private: bool,
) -> AxResult<isize> {
    let address = uaddr.addr();
    let tid = current().as_thread().tid();

    loop {
        let (key, namespace) = futex_key_from(address, private, &caller_aspace);
        let expected_key = key.shared_key().cloned();
        let table = futex_table_for(&key);
        let futex = table.get_or_insert_owned(&key);
        let pi_state = futex.existing_pi_state();

        let mut failure: Option<AxError> = None;
        let publish = |next_owner: Option<u32>| -> bool {
            let observed =
                match nofault_u32_read(address, &caller_aspace, namespace, expected_key.as_ref()) {
                    Ok(observed) => observed,
                    Err(_) => return false,
                };
            match plan_pi_unlock(PiWord::decode(observed), tid, next_owner) {
                PiUnlock::NotOwner => {
                    failure = Some(LinuxError::EPERM.into());
                    false
                }
                PiUnlock::Handoff { new_value } => futex_cas_at(
                    address,
                    namespace,
                    expected_key.as_ref(),
                    observed,
                    new_value,
                    &caller_aspace,
                )
                .unwrap_or(false),
                PiUnlock::Clear { .. } => futex_cas_at(
                    address,
                    namespace,
                    expected_key.as_ref(),
                    observed,
                    0,
                    &caller_aspace,
                )
                .unwrap_or(false),
            }
        };

        match futex.wq.pi_unlock(publish) {
            PiUnlockOutcome::HandedOff(next_owner) => {
                if let Some(pi_state) = pi_state {
                    // `wake_futex_pi()` drops the unlocking task's boost before
                    // the promoted waiter runs, then records the new owner.
                    pi_deboost_owner(&pi_state);
                    pi_state.attach(next_owner);
                }
                return Ok(0);
            }
            PiUnlockOutcome::Cleared => {
                if let Some(pi_state) = pi_state {
                    // Nobody is left to inherit the boost, so the owner's own
                    // scheduling state is restored.
                    pi_deboost_owner(&pi_state);
                    pi_state.detach();
                }
                futex.release_pi_state_if_idle();
                return Ok(0);
            }
            PiUnlockOutcome::Aborted => {
                if let Some(error) = failure {
                    return Err(error);
                }
                let _ = fault_read_u32(caller, address)?;
            }
        }
    }
}

/// `FUTEX_WAIT_REQUEUE_PI`.
///
/// Linux v7.2.3, `kernel/futex/requeue.c`:
///
/// ```c
/// int futex_wait_requeue_pi(u32 __user *uaddr, unsigned int flags,
/// 			  u32 val, ktime_t *abs_time, u32 bitset,
/// 			  u32 __user *uaddr2)
/// {
/// 	...
/// 	if (uaddr == uaddr2)
/// 		return -EINVAL;
/// ```
///
/// The waiter does not own `uaddr`; it blocks in `uaddr`'s queue carrying its
/// `rt_mutex_waiter` payload until `FUTEX_CMP_REQUEUE_PI` promotes it to owner
/// of `uaddr2`, at which point `*uaddr2` names it and the call returns 0. The
/// word is read from `uaddr` and compared with `val` before queueing, a
/// timeout expires as `-ETIMEDOUT`, and a signal leaves the requeue target
/// untouched so the waiter can restart the operation.
#[expect(clippy::too_many_arguments)]
fn do_futex_wait_requeue_pi(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    caller: &UserMemoryCapability,
    uaddr: *const u32,
    uaddr2: *const u32,
    value: u32,
    private: bool,
    deadline: Option<FutexWaitDeadline>,
) -> AxResult<isize> {
    let source = uaddr.addr();
    let target = uaddr2.addr();
    if source == target {
        return Err(AxError::InvalidInput);
    }
    let tid = current().as_thread().tid();

    // `futex_wait_requeue_pi()` resolves and validates `uaddr2` for writing
    // before it validates `*uaddr`, so an inaccessible target is EFAULT even
    // when the source comparison would already have failed.
    validate_futex_word_read(uaddr2, size_of::<u32>(), caller)?;
    let observed_target = fault_read_u32(caller, target)?;
    let target_word = PiWord::decode(observed_target);
    if target_word.tid == tid {
        return Err(LinuxError::EDEADLK.into());
    }

    loop {
        let observed = fault_read_u32(caller, source)?;
        if observed != value {
            return Err(AxError::WouldBlock);
        }

        let (source_key, source_namespace) = futex_key_from(source, private, &caller_aspace);
        let source_expected = source_key.shared_key().cloned();
        let source_table = futex_table_for(&source_key);
        let source_futex = source_table.get_or_insert_owned(&source_key);

        let (target_key, target_namespace) = futex_key_from(target, private, &caller_aspace);
        let target_expected = target_key.shared_key().cloned();
        let target_table = futex_table_for(&target_key);
        let target_futex = target_table.get_or_insert_owned(&target_key);
        let target_pi_state = target_futex.pi_state();
        target_pi_state.attach(target_word.tid);

        let result = source_futex.wq.wait_pi(
            source_futex.waiter_owner(),
            u32::MAX,
            deadline.map(|deadline| (deadline.clock, deadline.deadline)),
            || {
                let observed = nofault_u32_read(
                    source,
                    &caller_aspace,
                    source_namespace,
                    source_expected.as_ref(),
                )?;
                if observed != value {
                    return Err(WaitConditionError::Fault(AxError::WouldBlock));
                }
                Ok(Some(PiWaiter::new(tid, axtask::sched_state(&current()))))
            },
            |waiter| {
                pi_boost_owner(&source_futex.pi_state(), waiter);
                Ok(())
            },
        );

        match result {
            Ok(false) => {
                // The publication condition always either waits or fails, so
                // this is a lost race with the target's unlock.
                let _ = fault_read_u32(caller, source)?;
            }
            Ok(true) => {
                let observed = fault_read_u32(caller, target)?;
                if PiWord::decode(observed).tid == tid {
                    target_pi_state.attach(tid);
                    return Ok(0);
                }
                if let Some(deadline) = deadline
                    && deadline.clock.now() >= deadline.deadline
                {
                    return Err(AxError::TimedOut);
                }
            }
            Err(WaitConditionError::Retry) => {
                let _ = fault_read_u32(caller, source)?;
                if requeue_mapping_check(private, target_namespace) {
                    let _ = fault_read_u32(caller, target)?;
                }
                let _ = &target_expected;
            }
            Err(WaitConditionError::Fault(error)) => return Err(error),
        }
    }
}

/// `FUTEX_CMP_REQUEUE_PI`.
///
/// Linux v7.2.3, `kernel/futex/requeue.c`:
///
/// ```c
/// int futex_requeue(u32 __user *uaddr1, unsigned int flags1,
/// 		  u32 __user *uaddr2, unsigned int flags2,
/// 		  int nr_wake, int nr_requeue, u32 *cmpval, int requeue_pi)
/// {
/// 	...
/// 	if (requeue_pi) {
/// 		if (uaddr1 == uaddr2)
/// 			return -EINVAL;
/// 		...
/// 		if (nr_wake != 1)
/// 			return -EINVAL;
/// 	}
/// 	...
/// 	if (requeue_pi && futex_match(&key1, &key2))
/// 		return -EINVAL;
/// ```
///
/// The source word must still hold `value` (`-EAGAIN` otherwise) and the
/// target must be uncontended: `futex_proxy_trylock_atomic()` takes it for the
/// first waiter only when the caller can become its owner, so a target that is
/// already owned is `-EAGAIN` and the waiters stay on the source queue. A
/// source word naming the caller is `-EDEADLK`, and a target word naming the
/// caller is `-EDEADLK` too.
#[expect(clippy::too_many_arguments)]
fn do_futex_cmp_requeue_pi(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    caller: &UserMemoryCapability,
    uaddr: *const u32,
    uaddr2: *const u32,
    value: u32,
    nr_wake: u32,
    nr_requeue: i32,
    private: bool,
) -> AxResult<isize> {
    let source = uaddr.addr();
    let target = uaddr2.addr();
    // `futex_requeue()`: requeueing PI onto the same address is meaningless.
    if source == target {
        return Err(AxError::InvalidInput);
    }
    // Waking more than one waiter would have to hand the target to several
    // owners; every real user (`pthread_cond_signal`/`_broadcast`) uses 1.
    if nr_wake != 1 {
        return Err(AxError::InvalidInput);
    }
    let nr_requeue = usize::try_from(nr_requeue).map_err(|_| AxError::InvalidInput)?;
    let tid = current().as_thread().tid();

    validate_futex_word_read(uaddr2, size_of::<u32>(), caller)?;
    let observed_source = fault_read_u32(caller, source)?;
    if observed_source != value {
        return Err(AxError::WouldBlock);
    }
    let observed_target = fault_read_u32(caller, target)?;
    let target_word = PiWord::decode(observed_target);
    if target_word.tid == tid {
        return Err(LinuxError::EDEADLK.into());
    }
    if target_word.is_owned() {
        // `futex_proxy_trylock_atomic()` only takes an uncontended target.
        return Err(AxError::WouldBlock);
    }

    loop {
        let (source_key, source_namespace) = futex_key_from(source, private, &caller_aspace);
        let source_expected = source_key.shared_key().cloned();
        let source_table = futex_table_for(&source_key);
        let source_futex = source_table.get_or_insert_owned(&source_key);
        let (target_key, target_namespace) = futex_key_from(target, private, &caller_aspace);
        let target_expected = target_key.shared_key().cloned();
        let target_table = futex_table_for(&target_key);
        let target_futex = target_table.get_or_insert_owned(&target_key);
        let target_pi_state = target_futex.pi_state();
        // `futex_match(&key1, &key2)`: comparing the user addresses is not
        // enough for shared futexes, because one page can be mapped at two
        // addresses. Requeueing PI onto the same word that way would make the
        // caller the owner of the futex it is waiting on.
        if let (Some(lhs), Some(rhs)) = (source_key.shared_key(), target_key.shared_key()) {
            if lhs.backing() == rhs.backing() && lhs.offset() == rhs.offset() {
                return Err(AxError::InvalidInput);
            }
        }
        let mut promoted: Option<u32> = None;
        let mut failure: Option<AxError> = None;

        let result = source_futex.wq.pi_requeue(
            &target_futex.wq,
            target_futex.waiter_owner(),
            nr_requeue,
            |top_tid| {
                let observed = match nofault_u32_read(
                    target,
                    &caller_aspace,
                    target_namespace,
                    target_expected.as_ref(),
                ) {
                    Ok(observed) => observed,
                    Err(_) => return false,
                };
                if PiWord::decode(observed).is_owned() {
                    failure = Some(AxError::WouldBlock);
                    return false;
                }
                if !futex_cas_at(
                    target,
                    target_namespace,
                    target_expected.as_ref(),
                    observed,
                    pi_handoff_value(top_tid),
                    &caller_aspace,
                )
                .unwrap_or(false)
                {
                    return false;
                }
                promoted = Some(top_tid);
                true
            },
        );

        match result {
            Some((woke, moved)) => {
                if let Some(top_tid) = promoted {
                    // The promoted waiter is the target's new owner; it is the
                    // one that will boost whoever contends on it next.
                    target_pi_state.attach(top_tid);
                }
                // The promoted waiter left the source queue, so the source
                // owner may no longer need its boost.
                source_futex.release_pi_state_if_idle();
                return Ok((woke + moved) as isize);
            }
            None => {
                if let Some(error) = failure {
                    return Err(error);
                }
                let _ = &source_expected;
                if requeue_mapping_check(private, source_namespace) {
                    let _ = fault_read_u32(caller, source)?;
                }
                if requeue_mapping_check(private, target_namespace) {
                    let _ = fault_read_u32(caller, target)?;
                }
            }
        }
    }
}

pub fn sys_futex(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    uaddr: *const u32,
    futex_op: u32,
    value: u32,
    timeout: *const timespec,
    uaddr2: *mut u32,
    value3: u32,
) -> AxResult<isize> {
    debug!(
        "sys_futex <= uaddr: {uaddr:?}, futex_op: {futex_op}, value: {value}, uaddr2: {uaddr2:?}, \
         value3: {value3}",
    );

    let caller = UserMemoryCapability::new(caller_aspace.clone());
    // `do_futex()`'s `cmd = op & FUTEX_CMD_MASK`.  Linux's `FUTEX_CMD_MASK` is
    // the *complement* of the four modifier bits, so an unrecognised opcode
    // keeps its high bits and falls through to `-ENOSYS` instead of being
    // truncated into a valid command.
    let op = LegacyOp::decode(futex_op);
    let private = op.private;
    let legacy_flags = Futex2Flags {
        private,
        numa: false,
        mpol: false,
    };

    // `sys_futex()` reads the user timespec *before* `do_futex()` runs, so a
    // faulting or out-of-range timeout is reported ahead of the `-ENOSYS`
    // gates below even though those gates come first in `do_futex()`.
    let wait_deadline = if !timeout.is_null() && op.has_timeout() {
        Some(futex_wait_deadline(op, timeout, &caller)?)
    } else {
        None
    };

    let Some(command) = op.command else {
        return Err(LinuxError::ENOSYS.into());
    };
    if op.rejected_with_enosys() {
        return Err(LinuxError::ENOSYS.into());
    }

    match command {
        FutexCommand::Wait | FutexCommand::WaitBitset => {
            let bitset = if command == FutexCommand::WaitBitset {
                value3
            } else {
                u32::MAX
            };
            if bitset == 0 {
                return Err(AxError::InvalidInput);
            }
            validate_futex_word_read(uaddr, size_of::<u32>(), &caller)?;
            if let Some(deadline) = wait_deadline {
                current()
                    .as_thread()
                    .install_restart_block(RestartBlock::FutexWait(FutexWaitRestart {
                        uaddr: uaddr.addr(),
                        expected: value,
                        bitset,
                        deadline: deadline.deadline,
                        clock: deadline.clock,
                        private,
                    }));
            }
            do_futex_wait(
                caller_aspace,
                &caller,
                uaddr,
                value,
                bitset,
                wait_deadline,
                private,
            )
        }
        FutexCommand::Wake | FutexCommand::WakeBitset => {
            let bitset = if command == FutexCommand::WakeBitset {
                value3
            } else {
                u32::MAX
            };
            if bitset == 0 {
                return Err(AxError::InvalidInput);
            }
            validate_futex_key_access(uaddr, legacy_flags, &caller)?;
            if op.robust_unlock {
                // `futex_robust_unlock()` zeroes the futex word before the
                // wakeup, then clears the pending list op.
                futex_robust_unlock_store(&caller, uaddr)?;
                futex_robust_list_clear_pending(&caller, uaddr2, op.robust_list32)?;
            }
            let count = wake_futex(
                uaddr.addr(),
                private,
                legacy_wake_count(value),
                bitset,
                &caller_aspace,
                &caller,
            )?;
            Ok(count as _)
        }
        FutexCommand::WakeOp => {
            validate_futex_key_access(uaddr, legacy_flags, &caller)?;
            validate_futex_user_range(uaddr2.cast_const(), size_of::<u32>())?;
            check_user_writable_with(&caller, uaddr2.addr(), size_of::<u32>())?;
            let operation = WakeOp::decode(value3).map_err(|_| LinuxError::ENOSYS)?;
            // Legacy WAKE_OP checks its signed limits after each wake: even a
            // zero or negative limit wakes one matching waiter when present.
            let wake_count = wake_op_count(value);
            let wake_count2 = wake_op_count(timeout.addr() as u32);
            loop {
                let (key, namespace) = futex_key_from(uaddr.addr(), private, &caller_aspace);
                let expected = key.shared_key().cloned();
                let table = futex_table_for(&key);
                let futex = table.get_or_insert_owned(&key);
                let (key2, namespace2) = futex_key_from(uaddr2.addr(), private, &caller_aspace);
                let expected2 = key2.shared_key().cloned();
                let table2 = futex_table_for(&key2);
                let futex2 = table2.get_or_insert_owned(&key2);
                let result = futex.wq.wake_op(wake_count, &futex2.wq, wake_count2, || {
                    let aspace = caller_aspace.try_lock().ok_or(WaitConditionError::Retry)?;
                    let convert = |error| match error {
                        crate::mm::UserU32NofaultError::Retry => WaitConditionError::Retry,
                        crate::mm::UserU32NofaultError::BadAddress => {
                            WaitConditionError::Fault(AxError::BadAddress)
                        }
                    };
                    if !private {
                        crate::mm::try_validate_futex_mapping_nofault_locked(
                            &aspace,
                            uaddr.addr(),
                            namespace,
                            expected.as_ref(),
                        )
                        .map_err(convert)?;
                        crate::mm::try_validate_futex_mapping_nofault_locked(
                            &aspace,
                            uaddr2.addr(),
                            namespace2,
                            expected2.as_ref(),
                        )
                        .map_err(convert)?;
                    }
                    crate::mm::try_update_user_u32_nofault_locked(
                        &aspace,
                        uaddr2.addr(),
                        namespace2,
                        expected2.as_ref(),
                        |old| operation.updated(old),
                    )
                    .map(|_| true)
                    .map_err(convert)
                });
                match result {
                    Ok(count) => return Ok(count as _),
                    Err(WaitConditionError::Retry) => {
                        let _ = fault_read_u32(&caller, uaddr.addr())?;
                        let _ = fault_read_u32(&caller, uaddr2.addr())?;
                    }
                    Err(WaitConditionError::Fault(error)) => return Err(error),
                }
            }
        }
        FutexCommand::Requeue | FutexCommand::CmpRequeue => {
            // `futex_requeue()` takes `val` as `nr_wake`, the raw `uaddr2`
            // *register* as `nr_requeue`, and for `FUTEX_CMP_REQUEUE` also a
            // deferred `val3` comparison value.
            let requeue_expected = if command == FutexCommand::CmpRequeue {
                Some(value3)
            } else {
                None
            };
            if requeue_expected.is_some() {
                validate_futex_word_read(uaddr, size_of::<u32>(), &caller)?;
            } else {
                validate_futex_key_access(uaddr, legacy_flags, &caller)?;
            }
            validate_futex_requeue_target(uaddr2.cast_const(), legacy_flags, &caller)?;
            assert_unsigned(value)?;
            let value2 = assert_unsigned(timeout.addr() as u32)? as usize;

            if requeue_expected.is_some() {
                let _ = fault_read_u32(&caller, uaddr.addr())?;
            }
            loop {
                // Resolve the process image before entering a CMP_REQUEUE
                // queue gate; the comparison callback is strictly nofault.
                let aspace = caller_aspace.clone();
                let (key, namespace) = futex_key_from(uaddr.addr(), private, &aspace);
                let expected_key = key.shared_key().cloned();
                let futex_table = futex_table_for(&key);
                let futex = futex_table.get_or_insert_owned(&key);
                let (key2, namespace2) = futex_key_from(uaddr2.addr(), private, &aspace);
                let expected_key2 = key2.shared_key().cloned();
                let table2 = futex_table_for(&key2);
                let futex2 = table2.get_or_insert_owned(&key2);

                if let Some(requeue_expected) = requeue_expected {
                    let result = futex.wq.wake_and_requeue_if(
                        value as usize,
                        value2,
                        &futex2.wq,
                        futex2.waiter_owner(),
                        u32::MAX,
                        || {
                            nofault_read_and_validate_pair(
                                uaddr.addr(),
                                namespace,
                                expected_key.as_ref(),
                                uaddr2.addr(),
                                namespace2,
                                expected_key2.as_ref(),
                                private,
                                requeue_expected,
                                &aspace,
                            )
                        },
                    );
                    match result {
                        Ok(Some(result)) => return Ok((result.0 + result.1) as isize),
                        Ok(None) => return Err(AxError::WouldBlock),
                        Err(WaitConditionError::Retry) => {
                            let _ = fault_read_u32(&caller, uaddr.addr())?;
                            if requeue_mapping_check(private, namespace2) {
                                let _ = fault_read_u32(&caller, uaddr2.addr())?;
                            }
                            continue;
                        }
                        Err(WaitConditionError::Fault(error)) => return Err(error),
                    }
                } else if !requeue_mapping_check(private, namespace)
                    && !requeue_mapping_check(private, namespace2)
                {
                    let result = futex.wq.wake_and_requeue(
                        value as usize,
                        value2,
                        &futex2.wq,
                        futex2.waiter_owner(),
                        u32::MAX,
                    );
                    return Ok((result.0 + result.1) as isize);
                } else {
                    let result = futex.wq.wake_and_requeue_if(
                        value as usize,
                        value2,
                        &futex2.wq,
                        futex2.waiter_owner(),
                        u32::MAX,
                        || {
                            nofault_validate_pair(
                                uaddr.addr(),
                                namespace,
                                expected_key.as_ref(),
                                private,
                                uaddr2.addr(),
                                namespace2,
                                expected_key2.as_ref(),
                                private,
                                &aspace,
                            )
                        },
                    );
                    match result {
                        Ok(Some(result)) => return Ok((result.0 + result.1) as isize),
                        Ok(None) => return Err(AxError::WouldBlock),
                        Err(WaitConditionError::Retry) => {
                            if requeue_mapping_check(private, namespace) {
                                let _ = fault_read_u32(&caller, uaddr.addr())?;
                            }
                            if requeue_mapping_check(private, namespace2) {
                                let _ = fault_read_u32(&caller, uaddr2.addr())?;
                            }
                            continue;
                        }
                        Err(WaitConditionError::Fault(error)) => return Err(error),
                    }
                }
            }
        }
        FutexCommand::LockPi | FutexCommand::LockPi2 => {
            do_futex_lock_pi(caller_aspace, &caller, uaddr, private, wait_deadline, false)
        }
        FutexCommand::TrylockPi => {
            do_futex_lock_pi(caller_aspace, &caller, uaddr, private, None, true)
        }
        FutexCommand::UnlockPi => {
            validate_futex_word_read(uaddr, size_of::<u32>(), &caller)?;
            let result = do_futex_unlock_pi(caller_aspace, &caller, uaddr, private);
            if op.robust_unlock && result.is_ok() {
                // `futex_unlock_pi()` clears the pending list op only after the
                // unlock itself succeeded.
                check_user_writable_with(&caller, uaddr.addr(), size_of::<u32>())?;
                futex_robust_list_clear_pending(&caller, uaddr2, op.robust_list32)?;
            }
            result
        }
        FutexCommand::WaitRequeuePi => {
            validate_futex_word_read(uaddr, size_of::<u32>(), &caller)?;
            do_futex_wait_requeue_pi(
                caller_aspace,
                &caller,
                uaddr,
                uaddr2.cast_const(),
                value,
                private,
                wait_deadline,
            )
        }
        FutexCommand::CmpRequeuePi => {
            validate_futex_word_read(uaddr, size_of::<u32>(), &caller)?;
            do_futex_cmp_requeue_pi(
                caller_aspace,
                &caller,
                uaddr,
                uaddr2.cast_const(),
                value3,
                value,
                timeout.addr() as u32 as i32,
                private,
            )
        }
        FutexCommand::Fd => Err(LinuxError::ENOSYS.into()),
    }
}

pub fn sys_get_robust_list(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    tid: u32,
    head: *mut *const robust_list_head,
    size: *mut usize,
) -> AxResult<isize> {
    let caller = UserMemoryCapability::new(caller_aspace);
    let current_task = current();
    let current_thread = current_task.as_thread();
    let current_tid = current_thread.tid();
    let tid = if tid == 0 {
        0
    } else {
        current_thread
            .pid_ns()
            .resolve_visible_pid(tid)
            .ok_or(AxError::NoSuchProcess)?
    };
    let (task, authorized_image) = match robust_list_access_mode(tid, current_tid) {
        None => (current_task.clone(), None),
        Some(mode) => {
            let task = get_visible_task(tid)?;
            let target = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
            let authorized = check_current_thread_ptrace_image_access(target, mode)?;
            (task, Some(authorized))
        }
    };
    let target = task.try_as_thread().ok_or(AxError::NoSuchProcess)?;
    let robust_head = target.robust_list_head() as *const robust_list_head;
    caller
        .write_bytes(head as usize, &(robust_head as usize).to_ne_bytes())
        .map_err(map_usercopy_error)?;
    caller
        .write_bytes(size as usize, &size_of::<robust_list_head>().to_ne_bytes())
        .map_err(map_usercopy_error)?;
    // Retain the exact credential/image authorization through both the target
    // read and userspace result publication. This prevents the caller from
    // authorizing one task image and then silently resampling another.
    drop(authorized_image);

    Ok(0)
}

fn robust_list_access_mode(tid: u32, current_tid: u32) -> Option<PtraceAccessMode> {
    (tid != 0 && tid != current_tid).then_some(PtraceAccessMode::ReadReal)
}

pub fn sys_set_robust_list(head: *const robust_list_head, size: usize) -> AxResult<isize> {
    if size != size_of::<robust_list_head>() {
        return Err(AxError::InvalidInput);
    }
    current().as_thread().set_robust_list_head(head.addr());

    Ok(0)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axsync::Mutex;
    use linux_raw_sys::general::{
        FUTEX2_MPOL, FUTEX2_NUMA, FUTEX2_PRIVATE, FUTEX2_SIZE_U16, FUTEX2_SIZE_U32,
        FUTEX2_SIZE_U64, futex_waitv,
    };
    use memory_addr::VirtAddr;
    use tk_linux_futex::{
        FUTEX_CLOCK_REALTIME, FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_PRIVATE_FLAG,
        FUTEX_ROBUST_UNLOCK, FUTEX_UNLOCK_PI, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE,
        FUTEX_WAKE_BITSET, FutexCommand, LegacyOp,
    };

    use super::{
        FUTEX_NO_NODE, FutexMappingNamespace, PtraceAccessMode, UserMemoryCapability,
        checked_user_array_address, futex2_core_flags, legacy_wake_count, robust_list_access_mode,
        validate_futex_address, validate_futex2_flags, validate_futex2_value, validate_numa_node,
        validate_waitv_entry,
    };

    #[test]
    fn legacy_wake_count_never_suppresses_the_first_wakeup() {
        // Linux `futex_wake()` wakes the first match and only then tests
        // `++ret >= nr_wake`, so 0 and every negative value still wake one
        // waiter and report one wakeup. The previous implementation returned 0
        // for an unsigned zero, which silently dropped the wakeup.
        assert_eq!(legacy_wake_count(0), 1);
        assert_eq!(legacy_wake_count(1), 1);
        assert_eq!(legacy_wake_count(4), 4);
        assert_eq!(legacy_wake_count(u32::MAX), 1);
        assert_eq!(legacy_wake_count(0x8000_0000), 1);
    }

    #[test]
    fn an_unknown_high_opcode_bit_is_enosys_not_a_truncated_command() {
        // `FUTEX_CMD_MASK` is the complement of the modifier bits, so bit 11
        // survives into `cmd` and the operation is unknown. Masking with the
        // legacy 0x7f field width would have turned this into `FUTEX_WAKE`.
        assert_eq!(
            LegacyOp::decode(FUTEX_WAKE).command,
            Some(FutexCommand::Wake)
        );
        assert_eq!(LegacyOp::decode(FUTEX_WAKE | 0x800).command, None);
        assert_eq!(
            LegacyOp::decode(FUTEX_WAKE | FUTEX_PRIVATE_FLAG).command,
            Some(FutexCommand::Wake)
        );
        // The two `do_futex()` gates, and the modifier bits they honour.
        let lock_pi_rt = LegacyOp::decode(FUTEX_LOCK_PI | FUTEX_CLOCK_REALTIME);
        assert!(lock_pi_rt.rejected_with_enosys());
        let lock_pi2_rt = LegacyOp::decode(FUTEX_LOCK_PI2 | FUTEX_CLOCK_REALTIME);
        assert!(!lock_pi2_rt.rejected_with_enosys());
        assert!(LegacyOp::decode(FUTEX_WAIT | FUTEX_ROBUST_UNLOCK).rejected_with_enosys());
        assert!(!LegacyOp::decode(FUTEX_WAKE | FUTEX_ROBUST_UNLOCK).rejected_with_enosys());
        assert!(!LegacyOp::decode(FUTEX_UNLOCK_PI | FUTEX_ROBUST_UNLOCK).rejected_with_enosys());
        // `FUTEX_WAIT` is the only relative timeout, and `FUTEX_LOCK_PI` is
        // pinned to CLOCK_REALTIME while `FUTEX_LOCK_PI2` is not.
        assert!(LegacyOp::decode(FUTEX_WAIT).timeout_is_relative());
        assert!(!LegacyOp::decode(FUTEX_WAIT_BITSET).timeout_is_relative());
        assert_eq!(
            LegacyOp::decode(FUTEX_LOCK_PI).timeout_clock(),
            tk_linux_futex::Clock::Realtime
        );
        assert_eq!(
            LegacyOp::decode(FUTEX_LOCK_PI2).timeout_clock(),
            tk_linux_futex::Clock::Monotonic
        );
    }

    #[test]
    fn futex2_numa_node_word_follows_the_linux_protocol() {
        // `get_futex_key()`: only FUTEX_NO_NODE and a possible node pass, and
        // the comparison happens before the write-back.
        assert_eq!(validate_numa_node(FUTEX_NO_NODE), Ok(0));
        assert_eq!(validate_numa_node(0), Ok(0));
        assert!(validate_numa_node(1).is_err());
    }

    #[test]
    fn futex_word_alignment_is_rejected_before_user_access() {
        assert!(validate_futex_address(0x1000usize as *const u32, 4).is_ok());
        assert_eq!(
            validate_futex_address(0x1001usize as *const u32, 4),
            Err(axerrno::AxError::InvalidInput)
        );
        // A FUTEX2_NUMA word is eight bytes wide, so `get_futex_key()`'s
        // `address % size` rule is stricter: 4 mod 8 is not a valid word.
        assert!(validate_futex_address(0x1000usize as *const u32, 8).is_ok());
        assert_eq!(
            validate_futex_address(0x1004usize as *const u32, 8),
            Err(axerrno::AxError::InvalidInput)
        );
    }

    #[test]
    fn requeue_fast_path_distinguishes_explicit_private_from_mapped_private() {
        // Explicit PRIVATE uses the address-only key form and does not inspect
        // or fault the target PTE while queue gates are held.
        assert!(!super::requeue_mapping_check(true, None));
        // A non-private operation on a private/COW VMA has an address-based
        // key too, but its namespace must still be checked before publish.
        assert!(super::requeue_mapping_check(
            false,
            Some(FutexMappingNamespace::Private)
        ));
        // An unmapped resolution is also not allowed to take the old private
        // fast path; the task-context retry will classify the eventual fault.
        assert!(super::requeue_mapping_check(
            false,
            Some(FutexMappingNamespace::Unmapped)
        ));
        assert!(!super::requeue_mapping_check(false, None));
        assert!(!super::requeue_mapping_check(
            true,
            Some(FutexMappingNamespace::Shared)
        ));
        assert!(super::requeue_mapping_check(
            false,
            Some(FutexMappingNamespace::Shared)
        ));
    }

    #[test]
    fn futex2_only_accepts_private_or_shared_u32_words() {
        assert_eq!(
            validate_futex2_flags(FUTEX2_SIZE_U32).map(|flags| flags.private),
            Ok(false)
        );
        assert_eq!(
            validate_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_PRIVATE).map(|flags| flags.private),
            Ok(true)
        );
        // FUTEX2_NUMA and FUTEX2_MPOL are accepted and reported, because Linux
        // v7.2.3 implements NUMA-aware key placement for both.
        let numa = validate_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_NUMA).unwrap();
        assert!(numa.numa && !numa.private && !numa.mpol);
        assert_eq!(numa.word_bytes(), 8);
        let mpol = validate_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_MPOL).unwrap();
        assert!(mpol.mpol && !mpol.numa);
        assert_eq!(mpol.word_bytes(), 4);
        for flags in [FUTEX2_SIZE_U16, FUTEX2_SIZE_U64, FUTEX2_SIZE_U32 | 0x10] {
            assert_eq!(
                validate_futex2_flags(flags),
                Err(axerrno::AxError::InvalidInput)
            );
        }
    }

    #[test]
    fn futex2_requeue_preserves_independent_endpoint_key_flags() {
        let shared = futex2_core_flags(FUTEX2_SIZE_U32).unwrap();
        let private = futex2_core_flags(FUTEX2_SIZE_U32 | FUTEX2_PRIVATE).unwrap();

        assert!(private.private);
        assert!(!shared.private);
        assert_eq!(
            futex2_core_flags(FUTEX2_SIZE_U32 | 0x10),
            Err(axerrno::AxError::InvalidInput)
        );
    }

    #[test]
    fn futex2_values_cannot_be_truncated_to_32_bits() {
        assert_eq!(validate_futex2_value(u32::MAX as u64), Ok(u32::MAX));
        assert_eq!(
            validate_futex2_value(u32::MAX as u64 + 1),
            Err(axerrno::AxError::InvalidInput)
        );
    }

    #[test]
    fn futex2_syscall_numbers_match_linux_64_bit_abis() {
        assert_eq!(syscalls::Sysno::futex_wake as usize, 454);
        assert_eq!(syscalls::Sysno::futex_wait as usize, 455);
        assert_eq!(syscalls::Sysno::futex_requeue as usize, 456);
    }

    #[test]
    fn legacy_negative_wake_count_is_one() {
        // `futex_wake()` counts with `if (++ret >= nr_wake) break;` *after*
        // waking the first match, so an unsigned `val` of 0 and every negative
        // value wake one waiter and report one wakeup. A zero limit is not a
        // no-op: `FUTEX_WAKE` is documented as "wake at least one".
        assert_eq!(super::legacy_wake_count(0), 1);
        assert_eq!(super::legacy_wake_count(1), 1);
        assert_eq!(super::legacy_wake_count(i32::MAX as u32), i32::MAX as usize);
        assert_eq!(super::legacy_wake_count((-1_i32) as u32), 1);
        assert_eq!(super::legacy_wake_count(i32::MIN as u32), 1);
    }

    #[test]
    fn waitv_entry_rejects_reserved_bits_and_wider_values() {
        let valid = futex_waitv {
            val: 7,
            uaddr: 0x1000,
            flags: FUTEX2_SIZE_U32 | FUTEX2_PRIVATE,
            __reserved: 0,
        };
        assert!(validate_waitv_entry(&valid).is_ok());

        let mut wider = valid;
        wider.val = u32::MAX as u64 + 1;
        assert_eq!(
            validate_waitv_entry(&wider),
            Err(axerrno::AxError::InvalidInput)
        );

        let mut reserved = valid;
        reserved.__reserved = 1;
        assert_eq!(
            validate_waitv_entry(&reserved),
            Err(axerrno::AxError::InvalidInput)
        );
    }

    #[test]
    fn waitv_descriptor_count_overflow_is_rejected_before_pointer_arithmetic() {
        let capability = UserMemoryCapability::new(Arc::new(Mutex::new(
            super::AddrSpace::new_empty(VirtAddr::from(0x1000), 0x1000).unwrap(),
        )));
        assert_eq!(
            checked_user_array_address(0x1000usize as *const futex_waitv, usize::MAX, &capability,),
            Err(axerrno::AxError::BadAddress)
        );
    }

    #[test]
    fn credential_caller_robust_list_uses_read_real_only_for_other_tid() {
        assert_eq!(robust_list_access_mode(0, 41), None);
        assert_eq!(robust_list_access_mode(41, 41), None);
        assert_eq!(
            robust_list_access_mode(42, 41),
            Some(PtraceAccessMode::ReadReal)
        );
    }
}
