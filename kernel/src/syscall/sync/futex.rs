use alloc::sync::Arc;
use core::time::Duration;

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::{
    FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE,
    FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, robust_list_head, timespec,
};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    task::{
        AlarmClock, AsThread, FutexKey, FutexWaitRestart, RestartBlock, futex_table_for,
        get_visible_task,
    },
    time::TimeValueLike,
};

#[derive(Clone, Copy)]
struct FutexWaitDeadline {
    clock: AlarmClock,
    deadline: Duration,
}

fn assert_unsigned(value: u32) -> AxResult<u32> {
    if (value as i32) < 0 {
        Err(AxError::InvalidInput)
    } else {
        Ok(value)
    }
}

fn futex_key_from(address: usize, private: bool) -> FutexKey {
    if private {
        FutexKey::new_private(address)
    } else {
        FutexKey::new_current(address)
    }
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
) -> AxResult<Option<FutexWaitDeadline>> {
    let Some(ts) = timeout.nullable() else {
        return Ok(None);
    };
    // FIXME: AnyBitPattern
    let ts = unsafe { ts.vm_read_uninit()?.assume_init() }.try_into_time_value()?;
    let clock = futex_wait_clock(futex_op);
    let deadline = if command == FUTEX_WAIT_BITSET {
        ts
    } else {
        clock.now().checked_add(ts).unwrap_or(Duration::MAX)
    };
    Ok(Some(FutexWaitDeadline { clock, deadline }))
}

fn do_futex_wait(
    uaddr: *const u32,
    value: u32,
    bitset: u32,
    timeout: Option<FutexWaitDeadline>,
    private: bool,
) -> AxResult<isize> {
    if uaddr.vm_read()? != value {
        return Err(AxError::WouldBlock);
    }

    let key = if private {
        FutexKey::new_private(uaddr.addr())
    } else {
        FutexKey::new_current(uaddr.addr())
    };
    let futex_table = futex_table_for(&key);
    let futex = futex_table.get_or_insert(&key);

    if !futex.wq.wait_if(
        Arc::downgrade(&futex),
        bitset,
        timeout.map(|it| (it.clock, it.deadline)),
        || Ok(uaddr.vm_read()? == value),
    )? {
        return Err(AxError::WouldBlock);
    }

    Ok(0)
}

pub(crate) fn restart_futex_wait(block: FutexWaitRestart) -> AxResult<isize> {
    do_futex_wait(
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

    let private = futex_op & FUTEX_PRIVATE_FLAG != 0;
    let command = futex_op & (FUTEX_CMD_MASK as u32);
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let bitset = if command == FUTEX_WAIT_BITSET {
                value3
            } else {
                u32::MAX
            };
            let timeout = futex_wait_deadline(command, futex_op, timeout)?;
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
            do_futex_wait(uaddr, value, bitset, timeout, private)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            let key = futex_key_from(uaddr.addr(), private);
            let futex_table = futex_table_for(&key);
            let futex = futex_table.get(&key);
            let mut count = 0;
            if let Some(futex) = futex {
                let bitset = if command == FUTEX_WAKE_BITSET {
                    value3
                } else {
                    u32::MAX
                };
                count = futex.wq.wake(value as _, bitset);
            }
            Ok(count as _)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            assert_unsigned(value)?;
            if command == FUTEX_CMP_REQUEUE && uaddr.vm_read()? != value3 {
                return Err(AxError::WouldBlock);
            }
            let value2 = assert_unsigned(timeout.addr() as u32)? as usize;

            let key = futex_key_from(uaddr.addr(), private);
            let futex_table = futex_table_for(&key);
            let futex = futex_table.get(&key);
            let key2 = futex_key_from(uaddr2.addr(), private);
            let table2 = futex_table_for(&key2);
            let futex2 = table2.get_or_insert(&key2);

            let mut count = 0;
            if let Some(futex) = futex {
                let (woken, moved) = futex.wq.wake_and_requeue(
                    value as _,
                    if value2 != 0 && Arc::as_ptr(&*futex) != Arc::as_ptr(&*futex2) {
                        value2
                    } else {
                        0
                    },
                    &futex2.wq,
                    Arc::downgrade(&futex2),
                    u32::MAX,
                );
                count = woken + moved;
            }
            Ok(count as _)
        }
        _ => Err(AxError::Unsupported),
    }
}

pub fn sys_get_robust_list(
    tid: u32,
    head: *mut *const robust_list_head,
    size: *mut usize,
) -> AxResult<isize> {
    let current_task = current();
    let current_thread = current_task.as_thread();
    let current_tid = current_thread.tid();
    let task = if tid == 0 || tid == current_tid {
        current_task.clone()
    } else {
        if current_thread.proc_data.euid() != 0 {
            return Err(AxError::OperationNotPermitted);
        }
        get_visible_task(tid)?
    };
    head.vm_write(task.as_thread().robust_list_head() as _)?;
    size.vm_write(size_of::<robust_list_head>())?;

    Ok(0)
}

pub fn sys_set_robust_list(head: *const robust_list_head, size: usize) -> AxResult<isize> {
    if size != size_of::<robust_list_head>() {
        return Err(AxError::InvalidInput);
    }
    current().as_thread().set_robust_list_head(head.addr());

    Ok(0)
}
