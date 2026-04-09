use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    ffi::c_long,
    sync::atomic::{AtomicU32, Ordering},
};

use axerrno::{AxError, AxResult};
use axtask::{AxTaskRef, TaskInner, WeakAxTaskRef, current};
use bytemuck::AnyBitPattern;
use kernel_guard::NoPreemptIrqSave;
use linux_raw_sys::general::{FUTEX_OWNER_DIED, FUTEX_TID_MASK, FUTEX_WAITERS, ROBUST_LIST_LIMIT};
use memory_addr::PhysAddr;
use spin::RwLock;
use starry_process::{Pid, ProcessGroup, Session, ZombieSnapshot};
use starry_signal::{SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr};
use weak_map::WeakMap;

use super::{
    AsThread, FutexKey, ProcessData, TaskUsage, TimerState, futex_table_for,
    send_signal_thread_inner, send_signal_to_process, send_signal_to_thread,
};
use crate::mm::{UserPtr, access_user_memory};

static TASK_TABLE: RwLock<WeakMap<Pid, WeakAxTaskRef>> = RwLock::new(WeakMap::new());
static TASK_ALIAS_TABLE: RwLock<WeakMap<Pid, WeakAxTaskRef>> = RwLock::new(WeakMap::new());

static PROCESS_TABLE: RwLock<WeakMap<Pid, Weak<ProcessData>>> = RwLock::new(WeakMap::new());

static PROCESS_GROUP_TABLE: RwLock<WeakMap<Pid, Weak<ProcessGroup>>> = RwLock::new(WeakMap::new());

static SESSION_TABLE: RwLock<WeakMap<Pid, Weak<Session>>> = RwLock::new(WeakMap::new());

/// Cleanup expired entries in the task tables.
///
/// This function is intended to be used during memory leak analysis to remove
/// possible noise caused by expired entries in the [`WeakMap`].
pub fn cleanup_task_tables() {
    TASK_TABLE.write().cleanup();
    TASK_ALIAS_TABLE.write().cleanup();
    PROCESS_TABLE.write().cleanup();
    PROCESS_GROUP_TABLE.write().cleanup();
    SESSION_TABLE.write().cleanup();
}

/// Add the task, the thread and possibly its process, process group and session
/// to the corresponding tables.
pub fn add_task_to_table(task: &AxTaskRef) {
    let tid = task.id().as_u64() as Pid;

    let mut task_table = TASK_TABLE.write();
    task_table.insert(tid, task);

    let proc_data = &task.as_thread().proc_data;
    let proc = &proc_data.proc;
    let pid = proc.pid();
    let mut proc_table = PROCESS_TABLE.write();
    if proc_table.contains_key(&pid) {
        return;
    }
    proc_table.insert(pid, proc_data);

    let pg = proc.group();
    let mut pg_table = PROCESS_GROUP_TABLE.write();
    if pg_table.contains_key(&pg.pgid()) {
        return;
    }
    pg_table.insert(pg.pgid(), &pg);

    let session = pg.session();
    let mut session_table = SESSION_TABLE.write();
    if session_table.contains_key(&session.sid()) {
        return;
    }
    session_table.insert(session.sid(), &session);
}

/// Adds an additional lookup key for a task.
pub fn add_task_alias(alias: Pid, task: &AxTaskRef) {
    TASK_ALIAS_TABLE.write().insert(alias, task);
}

/// Lists all tasks.
pub fn tasks() -> Vec<AxTaskRef> {
    TASK_TABLE.read().values().collect()
}

/// Finds the task with the given TID.
pub fn get_task(tid: Pid) -> AxResult<AxTaskRef> {
    if tid == 0 {
        return Ok(current().clone());
    }
    TASK_TABLE
        .read()
        .get(&tid)
        .or_else(|| TASK_ALIAS_TABLE.read().get(&tid))
        .ok_or(AxError::NoSuchProcess)
}

/// Finds the task with the given user-visible TID.
pub fn get_visible_task(tid: Pid) -> AxResult<AxTaskRef> {
    if let Some(task) = TASK_ALIAS_TABLE.read().get(&tid)
        && task.as_thread().tid() == tid
        && !task.as_thread().pending_exit()
    {
        return Ok(task);
    }

    if let Some(task) = TASK_TABLE.read().get(&tid)
        && task.as_thread().tid() == tid
        && !task.as_thread().pending_exit()
    {
        return Ok(task);
    }

    Err(AxError::NoSuchProcess)
}

/// Removes a task alias lookup key.
pub fn remove_task_alias(alias: Pid) {
    TASK_ALIAS_TABLE.write().remove(&alias);
}

/// Lists all processes.
pub fn processes() -> Vec<Arc<ProcessData>> {
    PROCESS_TABLE.read().values().collect()
}

/// Finds the process with the given PID.
pub fn get_process_data(pid: Pid) -> AxResult<Arc<ProcessData>> {
    if pid == 0 {
        return Ok(current().as_thread().proc_data.clone());
    }
    PROCESS_TABLE.read().get(&pid).ok_or(AxError::NoSuchProcess)
}

/// Finds the process group with the given PGID.
pub fn get_process_group(pgid: Pid) -> AxResult<Arc<ProcessGroup>> {
    PROCESS_GROUP_TABLE
        .read()
        .get(&pgid)
        .ok_or(AxError::NoSuchProcess)
}

/// Finds the session with the given SID.
pub fn get_session(sid: Pid) -> AxResult<Arc<Session>> {
    SESSION_TABLE.read().get(&sid).ok_or(AxError::NoSuchProcess)
}

/// Updates the current task's saved and active user page table root.
pub fn set_current_user_page_table_root(root: PhysAddr) {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    // SAFETY: this only mutates the current task's saved TaskContext while the task
    // is running on the current CPU, so no other code can access it concurrently.
    let curr_ptr = (&***curr) as *const TaskInner as *mut TaskInner;
    unsafe {
        (*curr_ptr).ctx_mut().set_page_table_root(root);
        axhal::asm::write_user_page_table(root);
    }
    #[cfg(not(target_arch = "x86_64"))]
    axhal::asm::flush_tlb(None);
}

#[cfg(target_arch = "loongarch64")]
fn with_current_task_ctx_mut<R>(f: impl FnOnce(&mut axhal::context::TaskContext) -> R) -> R {
    let _guard = NoPreemptIrqSave::new();
    let curr = current();
    let curr_ptr = (&***curr) as *const TaskInner as *mut TaskInner;
    unsafe { f((*curr_ptr).ctx_mut()) }
}

/// Restores the current task's saved user FPU state to the CPU.
#[cfg(target_arch = "loongarch64")]
pub fn restore_current_user_fpu_state() {
    with_current_task_ctx_mut(|ctx| {
        ctx.fpu.restore();
    });
}

/// Saves the CPU's current FPU state into the current task's saved context.
#[cfg(target_arch = "loongarch64")]
pub fn save_current_user_fpu_state() {
    with_current_task_ctx_mut(|ctx| {
        ctx.fpu.save();
    });
}

/// Resets the current task's saved FPU state and restores the reset state to the CPU.
#[cfg(target_arch = "loongarch64")]
pub fn reset_current_user_fpu_state() {
    with_current_task_ctx_mut(|ctx| {
        ctx.fpu = Default::default();
        ctx.fpu.restore();
    });
}

/// Poll the timer
pub fn poll_timer(task: &TaskInner) {
    let Some(thr) = task.try_as_thread() else {
        return;
    };
    let Ok(mut time) = thr.time.try_borrow_mut() else {
        // reentrant borrow, likely IRQ
        return;
    };
    time.poll(|signo| {
        send_signal_thread_inner(task, thr, SignalInfo::new_kernel(signo));
    });
}

/// Sets the timer state.
pub fn set_timer_state(task: &TaskInner, state: TimerState) {
    let Some(thr) = task.try_as_thread() else {
        return;
    };
    let Ok(mut time) = thr.time.try_borrow_mut() else {
        // reentrant borrow, likely IRQ
        return;
    };
    time.poll(|signo| {
        send_signal_thread_inner(task, thr, SignalInfo::new_kernel(signo));
    });
    time.set_state(state);
}

#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustList {
    pub next: *mut RobustList,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustListHead {
    pub list: RobustList,
    pub futex_offset: c_long,
    pub list_op_pending: *mut RobustList,
}

fn handle_futex_death(entry: *mut RobustList, offset: i64) -> AxResult<()> {
    let address = (entry as u64)
        .checked_add_signed(offset)
        .ok_or(AxError::InvalidInput)?;
    let address: usize = address.try_into().map_err(|_| AxError::InvalidInput)?;
    let uaddr = address as *mut u32;
    if !mark_robust_owner_died(uaddr, current().as_thread().tid())? {
        return Ok(());
    }

    let key = FutexKey::new_current(address);
    let futex_table = futex_table_for(&key);
    if let Some(futex) = futex_table.get(&key) {
        futex.wq.wake(1, u32::MAX);
    }
    Ok(())
}

fn robust_owner_died_word(value: u32, tid: Pid) -> Option<u32> {
    if value & FUTEX_TID_MASK != tid {
        return None;
    }
    Some((value & FUTEX_WAITERS) | FUTEX_OWNER_DIED)
}

fn user_atomic_u32(uaddr: *mut u32) -> AxResult<&'static AtomicU32> {
    Ok(UserPtr::<AtomicU32>::from(uaddr.cast()).get_as_mut()?)
}

fn mark_robust_owner_died(uaddr: *mut u32, tid: Pid) -> AxResult<bool> {
    let word = user_atomic_u32(uaddr)?;
    let mut value = access_user_memory(|| word.load(Ordering::Acquire));
    loop {
        let Some(next_value) = robust_owner_died_word(value, tid) else {
            return Ok(false);
        };
        match access_user_memory(|| {
            word.compare_exchange(value, next_value, Ordering::AcqRel, Ordering::Acquire)
        }) {
            Ok(_) => return Ok(true),
            Err(observed) => value = observed,
        }
    }
}

fn is_robust_list_error(err: AxError) -> bool {
    matches!(
        err,
        AxError::BadAddress | AxError::InvalidInput | AxError::FilesystemLoop
    )
}

pub fn exit_robust_list(head: *const RobustListHead) {
    // Reference: https://elixir.bootlin.com/linux/v6.13.6/source/kernel/futex/core.c#L777

    let mut limit = ROBUST_LIST_LIMIT;

    let end_ptr = unsafe { &raw const (*head).list };
    let head = match head.vm_read() {
        Ok(head) => head,
        Err(err) if is_robust_list_error(err.into()) => return,
        Err(_) => return,
    };
    let mut entry = head.list.next;
    let offset = head.futex_offset;
    let pending = head.list_op_pending;

    while !core::ptr::eq(entry, end_ptr) {
        let next_entry = match entry.vm_read() {
            Ok(next) => next.next,
            Err(err) if is_robust_list_error(err.into()) => break,
            Err(_) => break,
        };
        if entry != pending {
            match handle_futex_death(entry, offset) {
                Ok(()) => {}
                Err(err) if is_robust_list_error(err) => break,
                Err(_) => break,
            }
        }
        entry = next_entry;

        limit -= 1;
        if limit == 0 {
            break;
        }
        axtask::yield_now();
    }

    if !pending.is_null() {
        let _ = handle_futex_death(pending, offset);
    }
}

pub fn do_exit(exit_code: i32, group_exit: bool) {
    let curr = current();
    let thr = curr.as_thread();
    let tid = curr.id().as_u64() as Pid;
    let visible_tid = thr.tid();

    info!("{} exit with code: {}", curr.id_name(), exit_code);

    let clear_child_tid = thr.clear_child_tid() as *mut u32;
    if clear_child_tid.vm_write(0).is_ok() {
        let key = FutexKey::new_current(clear_child_tid as usize);
        let table = futex_table_for(&key);
        let guard = table.get(&key);
        if let Some(futex) = guard {
            futex.wq.wake(1, u32::MAX);
        }
    }
    let head = thr.robust_list_head() as *const RobustListHead;
    if !head.is_null() {
        exit_robust_list(head);
    }

    let process = &thr.proc_data.proc;
    set_timer_state(&curr, TimerState::Kernel);
    thr.proc_data
        .account_exited_thread(TaskUsage::from_thread(thr));
    if visible_tid != tid {
        remove_task_alias(visible_tid);
    }
    thr.proc_data.end_exec(tid);
    let process_exited = process.exit_thread(tid, exit_code);
    if process_exited {
        process.publish_zombie_snapshot(ZombieSnapshot {
            wait_status: process.exit_code(),
            self_usage: thr.proc_data.self_usage().into(),
            child_usage: thr.proc_data.children_usage().into(),
            uid: thr.proc_data.uid(),
        });
        process.exit();
        if let Some(parent) = process.parent() {
            if let Some(signo) = thr.proc_data.exit_signal {
                let _ = send_signal_to_process(parent.pid(), Some(SignalInfo::new_kernel(signo)));
            }
            if let Ok(data) = get_process_data(parent.pid()) {
                data.child_exit_event.wake();
            }
        }
        thr.proc_data.exit_event.wake();
        thr.proc_data.release_vfork();

        crate::syscall::SHM_MANAGER
            .lock()
            .clear_proc_shm(process.pid());
    }
    thr.proc_data.exec_event.wake();
    thr.exit_event.wake();

    if group_exit && !process.is_group_exited() {
        process.group_exit();
        let sig = SignalInfo::new_kernel(Signo::SIGKILL);
        for tid in process.threads() {
            let _ = send_signal_to_thread(None, tid, Some(sig.clone()));
        }
    }
    thr.set_exit();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robust_owner_died_marks_matching_tid() {
        let tid = 42;
        assert_eq!(
            robust_owner_died_word(FUTEX_WAITERS | tid, tid),
            Some(FUTEX_WAITERS | FUTEX_OWNER_DIED)
        );
    }

    #[test]
    fn robust_owner_died_ignores_other_owner() {
        assert_eq!(robust_owner_died_word(7, 42), None);
    }
}
