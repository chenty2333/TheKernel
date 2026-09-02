use alloc::{sync::Arc, vec::Vec};
use core::time::Duration;

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{
    __kernel_clockid_t, CLOCK_MONOTONIC, CLOCK_REALTIME, FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK,
    FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT, FUTEX_WAIT_BITSET,
    FUTEX_WAITV_MAX, FUTEX_WAKE, FUTEX_WAKE_BITSET, futex_waitv, robust_list_head, timespec,
};
use thekernel_linux_futex::{
    Futex2Flags, FutexWaitV, parse_futex2_flags, plan_requeue, validate_requeue_flags,
};

use crate::{
    mm::{
        AddrSpace, FutexMappingNamespace, SharedFutexKey, UserMemoryCapability,
        check_user_readable_with, map_usercopy_error,
    },
    task::{
        AlarmClock, AsThread, FutexHandle, FutexKey, FutexWaitRestart, PtraceAccessMode,
        RestartBlock, WaitConditionError, WaitConditionResult,
        check_current_thread_ptrace_image_access, futex_table_for, get_visible_task,
        wait_on_any_futex_if_atomic,
    },
    time::TimeValueLike,
};

#[derive(Clone, Copy)]
struct FutexWaitDeadline {
    clock: AlarmClock,
    deadline: Duration,
}

fn validate_futex2_flags(flags: u32) -> AxResult<bool> {
    Ok(futex2_core_flags(flags)?.private)
}

fn validate_futex2_value(value: u64) -> AxResult<u32> {
    u32::try_from(value).map_err(|_| AxError::InvalidInput)
}

fn futex2_core_flags(flags: u32) -> AxResult<Futex2Flags> {
    // Queueing only supports native 32-bit futexes.  The ABI crate owns the
    // wire flag grammar and rejects unsupported NUMA/MPOL/size combinations
    // before this layer touches user memory or a wait queue.
    parse_futex2_flags(flags).map_err(|_| AxError::InvalidInput)
}

fn assert_unsigned(value: u32) -> AxResult<u32> {
    if (value as i32) < 0 {
        Err(AxError::InvalidInput)
    } else {
        Ok(value)
    }
}

fn legacy_wake_count(value: u32) -> usize {
    let signed = value as i32;
    if signed < 0 { 1 } else { signed as usize }
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

fn validate_futex_address(address: *const u32) -> AxResult<()> {
    if address.is_aligned() {
        Ok(())
    } else {
        Err(AxError::InvalidInput)
    }
}

fn validate_futex_user_range(address: *const u32, _caller: &UserMemoryCapability) -> AxResult<()> {
    validate_futex_address(address)?;
    crate::mm::check_access(address.addr(), size_of::<u32>())?;
    Ok(())
}

fn validate_futex_word_read(address: *const u32, caller: &UserMemoryCapability) -> AxResult<()> {
    validate_futex_address(address)?;
    check_user_readable_with(caller, address.addr(), size_of::<u32>())?;
    Ok(())
}

fn validate_futex_key_access(
    address: *const u32,
    private: bool,
    caller: &UserMemoryCapability,
) -> AxResult<()> {
    if private {
        // A private futex key is the process/address pair. Linux only applies
        // access_ok() here: FUTEX_WAKE on an in-range unmapped or PROT_NONE
        // address can therefore report zero waiters without faulting the
        // page. Shared keys still have to resolve their backing mapping.
        validate_futex_user_range(address, caller)
    } else {
        validate_futex_word_read(address, caller)
    }
}

/// Requeue never dereferences a process-private target.  Its key is exactly
/// `(mm, address)`, so Linux requires alignment but deliberately permits low
/// or currently unmapped addresses.  A shared target still has to resolve the
/// backing mapping from the user word.
fn validate_futex_requeue_target(
    address: *const u32,
    private: bool,
    caller: &UserMemoryCapability,
) -> AxResult<()> {
    if private {
        validate_futex_address(address)
    } else {
        validate_futex_word_read(address, caller)
    }
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
            return Ok(futex.map_or(0, |futex| futex.wq.wake(wake_count, bitset)));
        }

        let futex_table = futex_table_for(&key);
        let futex = futex_table.get(&key);
        return Ok(futex.map_or(0, |futex| futex.wq.wake(wake_count, bitset)));
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

fn futex_wait_clock(futex_op: u32) -> AlarmClock {
    if futex_op & FUTEX_CLOCK_REALTIME != 0 {
        AlarmClock::Realtime
    } else {
        AlarmClock::Monotonic
    }
}

fn futex_wait_deadline(
    command: u32,
    futex_op: u32,
    timeout: *const timespec,
    caller: &UserMemoryCapability,
) -> AxResult<Option<FutexWaitDeadline>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let ts = timeout;
    let ts = caller.read_value_uninit(ts).map_err(map_usercopy_error)?;
    // SAFETY: the explicit usercopy initialized the complete timespec.
    let ts = unsafe { ts.assume_init() }.try_into_time_value()?;
    let clock = futex_wait_clock(futex_op);
    let deadline = if command == FUTEX_WAIT_BITSET {
        ts
    } else {
        clock.now().checked_add(ts).unwrap_or(Duration::MAX)
    };
    Ok(Some(FutexWaitDeadline { clock, deadline }))
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
            let private = waiter.flags & FUTEX_PRIVATE_FLAG != 0;
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

    let private = validate_futex2_flags(flags)?;
    let caller = UserMemoryCapability::new(caller_aspace.clone());
    let mask = validate_futex2_value(mask)?;
    if mask == 0 {
        return Err(AxError::InvalidInput);
    }

    // Linux still resolves the futex key before the strict zero-wake fast
    // path, so full user-range/access validation happens even when no waiter
    // can be woken.
    validate_futex_key_access(uaddr, private, &caller)?;
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
    let private = validate_futex2_flags(flags)?;
    let value = validate_futex2_value(value)?;
    let mask = validate_futex2_value(mask)?;
    if mask == 0 {
        return Err(AxError::InvalidInput);
    }
    validate_futex_word_read(uaddr, &caller)?;
    let timeout = validate_waitv_timeout(timeout, clockid, &caller)?;
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
    let source_uaddr = plan.source.address as *const u32;
    let target_uaddr = plan.target.address as *const u32;
    let _ = fault_read_u32(&caller, source_uaddr.addr())?;
    validate_futex_requeue_target(target_uaddr, target_private, &caller)?;

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
    let private = futex_op & FUTEX_PRIVATE_FLAG != 0;
    let command = futex_op & (FUTEX_CMD_MASK as u32);
    if futex_op & FUTEX_CLOCK_REALTIME != 0 && command != FUTEX_WAIT_BITSET {
        return Err(LinuxError::ENOSYS.into());
    }
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if command == FUTEX_WAIT_BITSET {
                value3
            } else {
                u32::MAX
            };
            if bitset == 0 {
                return Err(AxError::InvalidInput);
            }
            validate_futex_word_read(uaddr, &caller)?;
            let timeout = futex_wait_deadline(command, futex_op, timeout, &caller)?;
            if let Some(timeout) = timeout {
                current()
                    .as_thread()
                    .install_restart_block(RestartBlock::FutexWait(FutexWaitRestart {
                        uaddr: uaddr.addr(),
                        expected: value,
                        bitset,
                        deadline: timeout.deadline,
                        clock: timeout.clock,
                        private,
                    }));
            }
            do_futex_wait(
                caller_aspace.clone(),
                &caller,
                uaddr,
                value,
                bitset,
                timeout,
                private,
            )
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let bitset = if command == FUTEX_WAKE_BITSET {
                value3
            } else {
                u32::MAX
            };
            if bitset == 0 {
                return Err(AxError::InvalidInput);
            }
            validate_futex_key_access(uaddr, private, &caller)?;
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
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            if command == FUTEX_CMP_REQUEUE {
                validate_futex_word_read(uaddr, &caller)?;
            } else {
                validate_futex_key_access(uaddr, private, &caller)?;
            }
            validate_futex_requeue_target(uaddr2.cast_const(), private, &caller)?;
            assert_unsigned(value)?;
            let value2 = assert_unsigned(timeout.addr() as u32)? as usize;

            if command == FUTEX_CMP_REQUEUE {
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

                if command == FUTEX_CMP_REQUEUE {
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
                                value3,
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
        _ => Err(AxError::Unsupported),
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

    use super::{
        FutexMappingNamespace, PtraceAccessMode, UserMemoryCapability, checked_user_array_address,
        futex2_core_flags, robust_list_access_mode, validate_futex_address, validate_futex2_flags,
        validate_futex2_value, validate_waitv_entry,
    };

    #[test]
    fn futex_word_alignment_is_rejected_before_user_access() {
        assert!(validate_futex_address(0x1000usize as *const u32).is_ok());
        assert_eq!(
            validate_futex_address(0x1001usize as *const u32),
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
        assert_eq!(validate_futex2_flags(FUTEX2_SIZE_U32), Ok(false));
        assert_eq!(
            validate_futex2_flags(FUTEX2_SIZE_U32 | FUTEX2_PRIVATE),
            Ok(true)
        );
        for flags in [
            FUTEX2_SIZE_U16,
            FUTEX2_SIZE_U64,
            FUTEX2_SIZE_U32 | FUTEX2_NUMA,
            FUTEX2_SIZE_U32 | FUTEX2_MPOL,
            FUTEX2_SIZE_U32 | 0x10,
        ] {
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
        assert_eq!(super::legacy_wake_count(0), 0);
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
