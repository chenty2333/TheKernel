//! Concrete, non-PMU perf sources.
//!
//! Static and dynamic source registration shared by tracefs, probes, and the
//! task-local `PerfGroup` producer.  Publication allocates only in syscall
//! context; trap delivery receives compact value descriptors and never parses
//! a path or takes the dynamic registry lock.

use alloc::sync::Arc;
use core::{
    arch::naked_asm,
    mem::offset_of,
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
};

use axcpu::trap::{BREAKPOINT, DEBUG, register_trap_handler};
use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;

use crate::{
    file::{
        PerfEvent,
        ResolveAtResult, resolve_at,
    },
    mm::{UserMemoryCapability, map_usercopy_error},
    task::AsThread,
    uprobe::UprobeFileKey,
};

/// Linux-compatible tracefs ID for `sched:sched_switch` in this kernel's
/// fixed tracepoint namespace. IDs are stable for a boot and are intentionally
/// exposed together with their format, just as tracefs does.
pub(crate) const SCHED_SWITCH_TRACEPOINT_ID: u64 = 1;
pub(crate) const RAW_SYSCALLS_ENTER_TRACEPOINT_ID: u64 = 2;
pub(crate) const RAW_SYSCALLS_EXIT_TRACEPOINT_ID: u64 = 3;
pub(crate) const SCHED_WAKEUP_TRACEPOINT_ID: u64 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TracepointInfo {
    pub(crate) id: u64,
    pub(crate) system: &'static str,
    pub(crate) name: &'static str,
    pub(crate) format: &'static str,
    /// Number of raw-tracepoint prototype argument slots (not tracefs bytes).
    pub(crate) raw_arg_count: u8,
}

const TRACEPOINTS: [TracepointInfo; 4] = [
    TracepointInfo {
        id: SCHED_WAKEUP_TRACEPOINT_ID,
        system: "sched",
        name: "sched_wakeup",
        format: "name: sched_wakeup\nID: 4\nformat:\n\tfield:unsigned short \
                 common_type;\toffset:0;\tsize:2;\tsigned:0;\n\tfield:unsigned char \
                 common_flags;\toffset:2;\tsize:1;\tsigned:0;\n\tfield:unsigned char \
                 common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n\tfield:int \
                 common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\tfield:char \
                 comm[16];\toffset:8;\tsize:16;\tsigned:1;\n\tfield:pid_t \
                 pid;\toffset:24;\tsize:4;\tsigned:1;\n\tfield:int \
                 prio;\toffset:28;\tsize:4;\tsigned:1;\n\tfield:int \
                 target_cpu;\toffset:32;\tsize:4;\tsigned:1;\n",
        raw_arg_count: 1,
    },
    TracepointInfo {
        id: SCHED_SWITCH_TRACEPOINT_ID,
        system: "sched",
        name: "sched_switch",
        format: "name: sched_switch\nID: 1\nformat:\n\tfield:unsigned short \
                 common_type;\toffset:0;\tsize:2;\tsigned:0;\n\tfield:unsigned char \
                 common_flags;\toffset:2;\tsize:1;\tsigned:0;\n\tfield:unsigned char \
                 common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n\tfield:int \
                 common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\tfield:char \
                 prev_comm[16];\toffset:8;\tsize:16;\tsigned:1;\n\tfield:pid_t \
                 prev_pid;\toffset:24;\tsize:4;\tsigned:1;\n\tfield:int \
                 prev_prio;\toffset:28;\tsize:4;\tsigned:1;\n\tfield:long \
                 prev_state;\toffset:32;\tsize:8;\tsigned:1;\n\tfield:char \
                 next_comm[16];\toffset:40;\tsize:16;\tsigned:1;\n\tfield:pid_t \
                 next_pid;\toffset:56;\tsize:4;\tsigned:1;\n\tfield:int \
                 next_prio;\toffset:60;\tsize:4;\tsigned:1;\n",
        raw_arg_count: 4,
    },
    TracepointInfo {
        id: RAW_SYSCALLS_ENTER_TRACEPOINT_ID,
        system: "raw_syscalls",
        name: "sys_enter",
        format: "name: sys_enter\nID: 2\nformat:\n\tfield:unsigned short \
                 common_type;\toffset:0;\tsize:2;\tsigned:0;\n\tfield:unsigned char \
                 common_flags;\toffset:2;\tsize:1;\tsigned:0;\n\tfield:unsigned char \
                 common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n\tfield:int \
                 common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\tfield:long \
                 id;\toffset:8;\tsize:8;\tsigned:1;\n\tfield:unsigned long \
                 args[6];\toffset:16;\tsize:48;\tsigned:0;\n",
        raw_arg_count: 2,
    },
    TracepointInfo {
        id: RAW_SYSCALLS_EXIT_TRACEPOINT_ID,
        system: "raw_syscalls",
        name: "sys_exit",
        format: "name: sys_exit\nID: 3\nformat:\n\tfield:unsigned short \
                 common_type;\toffset:0;\tsize:2;\tsigned:0;\n\tfield:unsigned char \
                 common_flags;\toffset:2;\tsize:1;\tsigned:0;\n\tfield:unsigned char \
                 common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n\tfield:int \
                 common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\tfield:long \
                 id;\toffset:8;\tsize:8;\tsigned:1;\n\tfield:long \
                 ret;\toffset:16;\tsize:8;\tsigned:1;\n",
        raw_arg_count: 2,
    },
];

const DYNAMIC_TRACEPOINT_SLOTS: usize = 32;
// Trace entry `common_type` is a u16. Keep dynamically allocated IDs in the
// ordinary tracefs range rather than inventing an ID which cannot be encoded
// in the RAW record advertised by its `format` file.
const DYNAMIC_TRACEPOINT_BASE: u64 = 5;
static DYNAMIC_TRACEPOINTS: [AtomicU64; DYNAMIC_TRACEPOINT_SLOTS] =
    [const { AtomicU64::new(0) }; DYNAMIC_TRACEPOINT_SLOTS];
/// Several `group/event` names may describe the same source. Keep that
/// source identity separately from the event-name registry key.
static DYNAMIC_TRACEPOINT_SOURCES: [AtomicU64; DYNAMIC_TRACEPOINT_SLOTS] =
    [const { AtomicU64::new(0) }; DYNAMIC_TRACEPOINT_SLOTS];
static DYNAMIC_TRACEPOINT_ENABLED: [AtomicU8; DYNAMIC_TRACEPOINT_SLOTS] =
    [const { AtomicU8::new(0) }; DYNAMIC_TRACEPOINT_SLOTS];
const KPROBE_SLOTS: usize = 32;
static KPROBES: [AtomicU64; KPROBE_SLOTS] = [const { AtomicU64::new(0) }; KPROBE_SLOTS];
static KPROBE_SAVED: [AtomicU8; KPROBE_SLOTS] = [const { AtomicU8::new(0) }; KPROBE_SLOTS];
static KPROBE_SINGLE_STEP: [AtomicU64; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicU64::new(0) }; axconfig::plat::MAX_CPU_NUM];
static KPROBE_SINGLE_STEP_ORIGINAL_TF: [AtomicU8; axconfig::plat::MAX_CPU_NUM] =
    [const { AtomicU8::new(0) }; axconfig::plat::MAX_CPU_NUM];
static KPROBE_RETURN_REFS: [AtomicU64; KPROBE_SLOTS] = [const { AtomicU64::new(0) }; KPROBE_SLOTS];
static KPROBE_RETIRING: [AtomicU8; KPROBE_SLOTS] = [const { AtomicU8::new(0) }; KPROBE_SLOTS];
const KPROBE_ADDRESS_MASK: u64 = (1u64 << 48) - 1;
const KPROBE_PATCHING: u64 = 1 << 63;
const KPROBE_REFS_MASK: u64 = 0x7fff;
const TRAP_FLAG: u64 = 1 << 8;
const KRETPROBE_INSTANCE_SLOTS: usize = 256;

#[derive(Clone, Copy)]
struct KretprobeInstance {
    task: u64,
    return_stack: u64,
    original_return: u64,
    function: u64,
}

impl KretprobeInstance {
    const EMPTY: Self = Self {
        task: 0,
        return_stack: 0,
        original_return: 0,
        function: 0,
    };
}

static KRETPROBE_INSTANCES: SpinNoIrq<[KretprobeInstance; KRETPROBE_INSTANCE_SLOTS]> =
    SpinNoIrq::new([KretprobeInstance::EMPTY; KRETPROBE_INSTANCE_SLOTS]);

/// A return address patched by the kretprobe entry owner lands on this
/// immutable kernel-text INT3. The #BP handler restores the exact saved RIP;
/// reaching UD2 means registry/stack ownership was corrupted and must not
/// silently continue.
#[unsafe(naked)]
unsafe extern "C" fn kretprobe_trampoline() -> ! {
    naked_asm!("int3", "ud2")
}

fn interrupted_kernel_stack(frame: &axcpu::TrapFrame) -> Option<u64> {
    if frame.cs & 3 != 0 {
        return None;
    }
    // Same-CPL exceptions do not push RSP/SS. The address at which those
    // synthetic TrapFrame fields would begin is therefore the interrupted
    // kernel RSP itself (immediately above RIP/CS/RFLAGS).
    Some(
        (frame as *const axcpu::TrapFrame as usize)
            .checked_add(offset_of!(axcpu::TrapFrame, rsp))? as u64,
    )
}

fn kprobe_return_active(address: u64) -> bool {
    kprobe_slot(address).is_some_and(|index| KPROBE_RETURN_REFS[index].load(Ordering::Acquire) != 0)
}

fn prepare_kretprobe_instance(frame: &mut axcpu::TrapFrame, function: u64) -> Option<usize> {
    if !kprobe_return_active(function) {
        return None;
    }
    let stack = interrupted_kernel_stack(frame)?;
    let original_return = unsafe {
        // SAFETY: for a same-CPL #BP `stack` is the interrupted kernel RSP;
        // a function-entry kprobe observes the ABI return word there.
        core::ptr::read_unaligned(stack as *const u64)
    };
    let trampoline = kretprobe_trampoline as *const () as u64;
    if original_return == 0 || original_return == trampoline {
        return None;
    }
    let return_stack = stack.checked_add(8)?;
    let current = axtask::current();
    let task = current.try_as_thread()?.kernel_tid() as u64;
    let mut instances = KRETPROBE_INSTANCES.lock();
    let slot = instances.iter().position(|instance| instance.task == 0)?;
    unsafe {
        // SAFETY: trap handling owns this live kernel stack word until iret;
        // publication remains under the instance lock so no return handler
        // can observe a half-written record.
        core::ptr::write_unaligned(stack as *mut u64, trampoline);
    }
    instances[slot] = KretprobeInstance {
        task,
        return_stack,
        original_return,
        function,
    };
    Some(slot)
}

fn rollback_kretprobe_instance(frame: &axcpu::TrapFrame, slot: Option<usize>) {
    let Some(slot) = slot else { return };
    let mut instances = KRETPROBE_INSTANCES.lock();
    let instance = instances[slot];
    if instance.task == 0 {
        return;
    }
    if let Some(stack) = interrupted_kernel_stack(frame)
        && stack.checked_add(8) == Some(instance.return_stack)
    {
        let trampoline = kretprobe_trampoline as *const () as u64;
        let live = unsafe { core::ptr::read_unaligned(stack as *const u64) };
        if live == trampoline {
            unsafe { core::ptr::write_unaligned(stack as *mut u64, instance.original_return) };
        }
    }
    instances[slot] = KretprobeInstance::EMPTY;
}

fn claim_kretprobe_return(frame: &mut axcpu::TrapFrame) -> bool {
    let trampoline = kretprobe_trampoline as *const () as u64;
    if frame.rip.wrapping_sub(1) != trampoline || frame.cs & 3 != 0 {
        return false;
    }
    let Some(stack) = interrupted_kernel_stack(frame) else {
        return false;
    };
    let current = axtask::current();
    let Some(thread) = current.try_as_thread() else {
        return false;
    };
    let task = thread.kernel_tid() as u64;
    let instance = {
        let mut instances = KRETPROBE_INSTANCES.lock();
        let Some(slot) = instances
            .iter()
            .rposition(|instance| instance.task == task && instance.return_stack == stack)
        else {
            return false;
        };
        let instance = instances[slot];
        instances[slot] = KretprobeInstance::EMPTY;
        instance
    };
    frame.rip = instance.original_return;
    if kprobe_return_active(instance.function) {
        let mut payload = [0u8; 24];
        trace_common(&mut payload[..8], 0);
        payload[8..16].copy_from_slice(&instance.function.to_ne_bytes());
        payload[16..24].copy_from_slice(&instance.original_return.to_ne_bytes());
        emit_current_raw_at(
            PerfEvent::Kprobe {
                addr: instance.function,
                retprobe: true,
                query_offset: 0,
            },
            instance.original_return,
            &payload,
        );
    }
    true
}

pub(crate) fn retire_kretprobe_task(task: u64) {
    let mut instances = KRETPROBE_INSTANCES.lock();
    for instance in instances
        .iter_mut()
        .filter(|instance| instance.task == task)
    {
        *instance = KretprobeInstance::EMPTY;
    }
}

fn kprobe_refs(slot: u64) -> u64 {
    (slot >> 48) & KPROBE_REFS_MASK
}

fn canonical_kprobe_address(key: u64) -> u64 {
    if key & (1 << 47) != 0 {
        key | !KPROBE_ADDRESS_MASK
    } else {
        key
    }
}

pub(crate) fn has_deferred_kprobe_restore_work() -> bool {
    KPROBE_RETIRING
        .iter()
        .any(|retiring| retiring.load(Ordering::Acquire) != 0)
}

pub(crate) fn drain_one_deferred_kprobe_restore() -> bool {
    for (index, retiring) in KPROBE_RETIRING.iter().enumerate() {
        if retiring.load(Ordering::Acquire) == 0 {
            continue;
        }
        let slot = &KPROBES[index];
        let mut old = slot.load(Ordering::Acquire);
        loop {
            if retiring.load(Ordering::Acquire) == 0 {
                return false;
            }
            if old & KPROBE_PATCHING != 0 || kprobe_refs(old) != 1 {
                return false;
            }
            if slot
                .compare_exchange(
                    old,
                    old | KPROBE_PATCHING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                break;
            }
            old = slot.load(Ordering::Acquire);
        }
        let address = canonical_kprobe_address(old & KPROBE_ADDRESS_MASK);
        let restored = crate::text_patch::TextPatchTransaction::begin(address as usize).and_then(
            |mut patch| {
                patch.replace_byte(
                    address as usize,
                    KPROBE_SAVED[index].load(Ordering::Acquire),
                )?;
                patch.commit()
            },
        );
        if restored.is_err() {
            slot.fetch_and(!KPROBE_PATCHING, Ordering::Release);
            return false;
        }
        KPROBE_RETURN_REFS[index].store(0, Ordering::Release);
        retiring.store(0, Ordering::Release);
        slot.store(0, Ordering::Release);
        crate::syscall::release_kprobe_address(address);
        return true;
    }
    false
}

fn kprobe_slot(address: u64) -> Option<usize> {
    let key = address & KPROBE_ADDRESS_MASK;
    KPROBES.iter().position(|slot| {
        let value = slot.load(Ordering::Acquire);
        value & KPROBE_ADDRESS_MASK == key && kprobe_refs(value) != 0
    })
}

/// Claim a bounded, lock-free lookup slot before the architecture patches the
/// instruction. The caller must publish the INT3 only after this succeeds.
pub(crate) fn register_kprobe(address: u64, retprobe: bool) -> AxResult<()> {
    if retprobe {
        crate::syscall::validate_kretprobe_address(address)?;
    }
    crate::syscall::retain_kprobe_address(address)?;
    for (index, slot) in KPROBES.iter().enumerate() {
        let key = address & KPROBE_ADDRESS_MASK;
        loop {
            let old = slot.load(Ordering::Acquire);
            let old_address = old & KPROBE_ADDRESS_MASK;
            if old & KPROBE_PATCHING != 0 {
                if old_address == key {
                    // Single-step temporarily restores the instruction.  A
                    // second registration for that source must join the same
                    // slot after rearm, never allocate a duplicate slot whose
                    // saved byte races the first owner.
                    core::hint::spin_loop();
                    continue;
                }
                break;
            }
            if old_address == key && kprobe_refs(old) != 0 {
                if KPROBE_RETIRING[index].load(Ordering::Acquire) != 0 {
                    if slot
                        .compare_exchange(
                            old,
                            old | KPROBE_PATCHING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    if KPROBE_RETIRING[index].swap(0, Ordering::AcqRel) != 0 {
                        // Adopt the deferred slot for this new external
                        // reference.  Its old module pin belonged to restore
                        // custody; the registration already acquired its own.
                        KPROBE_RETURN_REFS[index].store(u64::from(retprobe), Ordering::Release);
                        slot.store(key | (1 << 48), Ordering::Release);
                        crate::syscall::release_kprobe_address(address);
                        return Ok(());
                    }
                    slot.fetch_and(!KPROBE_PATCHING, Ordering::Release);
                    continue;
                }
                let refs = kprobe_refs(old);
                if refs == KPROBE_REFS_MASK {
                    crate::syscall::release_kprobe_address(address);
                    return Err(AxError::StorageFull);
                }
                if slot
                    .compare_exchange(
                        old,
                        old_address | ((refs + 1) << 48),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }
                if retprobe {
                    KPROBE_RETURN_REFS[index].fetch_add(1, Ordering::AcqRel);
                }
                return Ok(());
            }
            if old != 0 {
                break;
            }
            if slot
                .compare_exchange(
                    0,
                    key | (1 << 48) | KPROBE_PATCHING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            let mut patch = match crate::text_patch::TextPatchTransaction::begin(address as usize) {
                Ok(patch) => patch,
                Err(error) => {
                    slot.store(0, Ordering::Release);
                    crate::syscall::release_kprobe_address(address);
                    return Err(error);
                }
            };
            let saved = match patch.replace_byte(address as usize, 0xcc) {
                Ok(byte) => byte,
                Err(error) => {
                    drop(patch);
                    slot.store(0, Ordering::Release);
                    crate::syscall::release_kprobe_address(address);
                    return Err(error);
                }
            };
            if saved == 0xcc {
                let _ = patch.replace_byte(address as usize, saved);
                let _ = patch.commit();
                slot.store(0, Ordering::Release);
                crate::syscall::release_kprobe_address(address);
                return Err(AxError::InvalidInput);
            }
            if let Err(error) = patch.commit() {
                slot.store(0, Ordering::Release);
                crate::syscall::release_kprobe_address(address);
                return Err(error);
            }
            KPROBE_SAVED[index].store(saved, Ordering::Release);
            KPROBE_RETURN_REFS[index].store(u64::from(retprobe), Ordering::Release);
            KPROBE_RETIRING[index].store(0, Ordering::Release);
            slot.store(key | (1 << 48), Ordering::Release);
            return Ok(());
        }
    }
    crate::syscall::release_kprobe_address(address);
    Err(AxError::StorageFull)
}

pub(crate) fn unregister_kprobe(address: u64, retprobe: bool) {
    for (index, slot) in KPROBES.iter().enumerate() {
        let key = address & KPROBE_ADDRESS_MASK;
        loop {
            let old = slot.load(Ordering::Acquire);
            if old & KPROBE_ADDRESS_MASK != key {
                break;
            }
            if old & KPROBE_PATCHING != 0 {
                // A #BP/#DB owner temporarily has the only matching slot.
                // Close must wait for that exact owner rather than advancing
                // to later slots and permanently losing this reference.
                core::hint::spin_loop();
                continue;
            }
            let refs = kprobe_refs(old);
            if refs != 0
                && slot
                    .compare_exchange(
                        old,
                        if refs == 1 {
                            key | KPROBE_PATCHING
                        } else {
                            key | ((refs - 1) << 48)
                        },
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                if retprobe {
                    let _ = KPROBE_RETURN_REFS[index].try_update(
                        Ordering::AcqRel,
                        Ordering::Acquire,
                        |refs| refs.checked_sub(1),
                    );
                }
                if refs == 1 {
                    let restored = crate::text_patch::TextPatchTransaction::begin(address as usize)
                        .and_then(|mut patch| {
                            patch.replace_byte(
                                address as usize,
                                KPROBE_SAVED[index].load(Ordering::Acquire),
                            )?;
                            patch.commit()
                        });
                    if restored.is_err() {
                        // Retain ownership of the live INT3 and publish it to
                        // the task-context restore worker.  This one internal
                        // reference is custody, not a phantom external event.
                        KPROBE_RETIRING[index].store(1, Ordering::Release);
                        slot.store(key | (1 << 48), Ordering::Release);
                        crate::deferred_work::wake_uprobe_restore_worker();
                        return;
                    }
                    KPROBE_RETURN_REFS[index].store(0, Ordering::Release);
                    KPROBE_RETIRING[index].store(0, Ordering::Release);
                    slot.store(0, Ordering::Release);
                }
                crate::syscall::release_kprobe_address(address);
                return;
            }
            if refs == 0 {
                break;
            }
        }
    }
}

fn kprobe_active(address: u64) -> bool {
    kprobe_slot(address).is_some()
}

/// Starts execution of the instruction displaced by a kprobe INT3.  The slot
/// remains patch-owned until #DB rearms INT3, so close cannot restore text in
/// the one-instruction window where the original byte is live.
fn kprobe_begin_single_step(frame: &mut axcpu::TrapFrame, address: u64) -> bool {
    let Some(index) = kprobe_slot(address) else {
        return false;
    };
    let slot = &KPROBES[index];
    let mut current = slot.load(Ordering::Acquire);
    loop {
        if current & KPROBE_PATCHING != 0 || current & KPROBE_ADDRESS_MASK != address {
            return false;
        }
        if slot
            .compare_exchange(
                current,
                current | KPROBE_PATCHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            break;
        }
        current = slot.load(Ordering::Acquire);
    }

    let restore =
        crate::text_patch::TextPatchTransaction::begin(address as usize).and_then(|mut patch| {
            patch.replace_byte(
                address as usize,
                KPROBE_SAVED[index].load(Ordering::Acquire),
            )?;
            patch.commit()
        });
    if restore.is_err() {
        slot.fetch_and(!KPROBE_PATCHING, Ordering::Release);
        return false;
    }

    let cpu = axhal::percpu::this_cpu_id();
    if cpu >= KPROBE_SINGLE_STEP.len()
        || KPROBE_SINGLE_STEP[cpu]
            .compare_exchange(0, address, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        // A nested debug trap cannot safely share the one preallocated state
        // record. Restore the INT3 before declining this exception.
        let _ = crate::text_patch::TextPatchTransaction::begin(address as usize).and_then(
            |mut patch| {
                patch.replace_byte(address as usize, 0xcc)?;
                patch.commit()
            },
        );
        slot.fetch_and(!KPROBE_PATCHING, Ordering::Release);
        return false;
    }
    KPROBE_SINGLE_STEP_ORIGINAL_TF[cpu]
        .store(u8::from(frame.rflags & TRAP_FLAG != 0), Ordering::Release);
    frame.rip = address;
    frame.rflags |= TRAP_FLAG;
    true
}

/// Returns `Some(true)` when the kprobe was layered over a pre-existing TF
/// and the architectural BS reason must remain available to the debugger.
fn kprobe_finish_single_step(frame: &mut axcpu::TrapFrame) -> Option<bool> {
    let cpu = axhal::percpu::this_cpu_id();
    if cpu >= KPROBE_SINGLE_STEP.len() {
        return None;
    }
    let address = KPROBE_SINGLE_STEP[cpu].swap(0, Ordering::AcqRel);
    if address == 0 {
        return None;
    }
    let original_tf = KPROBE_SINGLE_STEP_ORIGINAL_TF[cpu].swap(0, Ordering::AcqRel) != 0;
    let Some(index) = kprobe_slot(address) else {
        if !original_tf {
            frame.rflags &= !TRAP_FLAG;
        }
        return Some(original_tf);
    };
    let slot = &KPROBES[index];
    let rearm =
        crate::text_patch::TextPatchTransaction::begin(address as usize).and_then(|mut patch| {
            patch.replace_byte(address as usize, 0xcc)?;
            patch.commit()
        });
    if rearm.is_ok() {
        slot.fetch_and(!KPROBE_PATCHING, Ordering::Release);
        if original_tf {
            frame.rflags |= TRAP_FLAG;
        } else {
            frame.rflags &= !TRAP_FLAG;
        }
    }
    // Claim #DB even if rearming failed: the slot remains patch-owned and
    // therefore close cannot remove its metadata underneath this CPU.
    Some(original_tf)
}

/// Registers a tracefs dynamic event key and returns its boot-local stable
/// numeric ID. The caller owns deletion through [`unregister_tracefs_event`].
/// The registry retains only a canonical key, not borrowed path/name storage.
pub(crate) fn register_tracefs_event(key: u64, source: PerfEvent) -> AxResult<u64> {
    if key == 0 {
        return Err(AxError::InvalidInput);
    }
    let source = dynamic_source_key(source);
    if source == 0 {
        return Err(AxError::InvalidInput);
    }
    for (index, slot) in DYNAMIC_TRACEPOINTS.iter().enumerate() {
        let old = slot.load(Ordering::Acquire);
        if old == key
            || (old == 0
                && slot
                    .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok())
        {
            DYNAMIC_TRACEPOINT_SOURCES[index].store(source, Ordering::Release);
            DYNAMIC_TRACEPOINT_ENABLED[index].store(1, Ordering::Release);
            return Ok(DYNAMIC_TRACEPOINT_BASE + index as u64);
        }
    }
    Err(AxError::StorageFull)
}

pub(crate) fn unregister_tracefs_event(id: u64) -> AxResult<()> {
    let index = dynamic_tracepoint_index(id)?;
    DYNAMIC_TRACEPOINT_ENABLED[index].store(0, Ordering::Release);
    DYNAMIC_TRACEPOINT_SOURCES[index].store(0, Ordering::Release);
    DYNAMIC_TRACEPOINTS[index].store(0, Ordering::Release);
    Ok(())
}

pub(crate) fn set_tracefs_event_enabled(id: u64, enabled: bool) -> AxResult<()> {
    let index = dynamic_tracepoint_index(id)?;
    if DYNAMIC_TRACEPOINTS[index].load(Ordering::Acquire) == 0 {
        return Err(AxError::NotFound);
    }
    DYNAMIC_TRACEPOINT_ENABLED[index].store(enabled as u8, Ordering::Release);
    Ok(())
}

fn dynamic_tracepoint_index(id: u64) -> AxResult<usize> {
    id.checked_sub(DYNAMIC_TRACEPOINT_BASE)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|index| *index < DYNAMIC_TRACEPOINT_SLOTS)
        .ok_or(AxError::InvalidInput)
}

pub(crate) fn tracepoint(id: u64) -> AxResult<TracepointInfo> {
    TRACEPOINTS
        .iter()
        .copied()
        .find(|entry| entry.id == id)
        .or_else(|| {
            id.checked_sub(DYNAMIC_TRACEPOINT_BASE)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|index| *index < DYNAMIC_TRACEPOINT_SLOTS)
                .filter(|index| DYNAMIC_TRACEPOINTS[*index].load(Ordering::Acquire) != 0)
                .map(|_| TracepointInfo {
                    id,
                    system: "dynamic",
                    name: "probe",
                    format: "name: probe\nformat:\n\tfield:u64 ip;\n",
                    raw_arg_count: 0,
                })
        })
        .ok_or(AxError::OperationNotSupported)
}

pub(crate) fn tracefs_id(system: &str, name: &str) -> Option<u64> {
    TRACEPOINTS
        .iter()
        .find(|entry| entry.system == system && entry.name == name)
        .map(|entry| entry.id)
}

/// Raw tracepoint names use the event name without the tracefs subsystem
/// prefix.  Reject ambiguity rather than binding a program to whichever
/// provider happened to be registered first.
pub(crate) fn raw_tracepoint(name: &[u8]) -> Option<TracepointInfo> {
    // Only syscall producers currently construct a typed, invocation-local
    // pt_regs-style snapshot. sched_switch has task pointers in its Linux
    // prototype and remains unavailable until it gains an equally bounded
    // typed task context instead of fabricated addresses.
    TRACEPOINTS.iter().copied().find(|entry| {
        matches!(
            entry.id,
            RAW_SYSCALLS_ENTER_TRACEPOINT_ID | RAW_SYSCALLS_EXIT_TRACEPOINT_ID
        ) && entry.name.as_bytes() == name
    })
}

pub(crate) fn tracefs_format(system: &str, name: &str) -> Option<&'static str> {
    TRACEPOINTS
        .iter()
        .find(|entry| entry.system == system && entry.name == name)
        .map(|entry| entry.format)
}

/// Resolves a copied uprobe pathname into the VFS object's stable
/// mount/device/inode key.  No transient user pointer or pathname hash enters
/// the registry, so an exec/dlopen remap of the same object matches exactly.
pub(crate) struct ResolvedUprobeFile {
    pub(crate) key: UprobeFileKey,
    pub(crate) name: Arc<alloc::vec::Vec<u8>>,
}

pub(crate) fn resolve_uprobe_inode(
    memory: &UserMemoryCapability,
    path: *const u8,
    _task_id: u64,
) -> AxResult<ResolvedUprobeFile> {
    if path.is_null() {
        return Err(AxError::BadAddress);
    }
    let mut bytes = alloc::vec::Vec::new();
    for offset in 0..4096usize {
        let address = (path as usize)
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        let byte = memory
            .read_value_uninit(address as *const u8)
            .map_err(map_usercopy_error)
            .map(|value| unsafe { value.assume_init() })?;
        if byte == 0 {
            if offset == 0 {
                return Err(AxError::InvalidInput);
            }
            let ResolveAtResult::File(location) = resolve_at(
                linux_raw_sys::general::AT_FDCWD,
                Some(axfs_ng_vfs::FsPath::new(&bytes)),
                0,
            )?
            else {
                return Err(AxError::InvalidInput);
            };
            return Ok(ResolvedUprobeFile {
                key: UprobeFileKey {
                    mount_id: location.mountpoint().mount_id(),
                    device: location.mountpoint().device(),
                    inode: location.inode(),
                },
                name: Arc::try_new(bytes).map_err(|_| AxError::NoMemory)?,
            });
        }
        bytes.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        bytes.push(byte);
    }
    Err(AxError::NameTooLong)
}

/// Tracefs control writes are kernel-originated strings rather than user
/// pointers. Resolve them through the same VFS identity path used by perf
/// open, so a dynamic uprobe never retains a pathname as its authority.
pub(crate) fn resolve_uprobe_path(path: &str) -> AxResult<UprobeFileKey> {
    let path = axfs_ng_vfs::FsPathBuf::from_vec(alloc::vec::Vec::from(path.as_bytes()));
    let ResolveAtResult::File(location) =
        resolve_at(linux_raw_sys::general::AT_FDCWD, Some(path.as_ref()), 0)?
    else {
        return Err(AxError::InvalidInput);
    };
    Ok(UprobeFileKey {
        mount_id: location.mountpoint().mount_id(),
        device: location.mountpoint().device(),
        inode: location.inode(),
    })
}

/// Delivers a dynamic source into the current task's unified perf groups.
/// It is usable from exception context because all registration and usercopy
/// happened before the event reached this path.
pub(crate) fn emit_current(event: PerfEvent) {
    // Return probes can lack a live instruction pointer; retain a valid
    // trace-entry prefix and append their compact source identity.
    let mut payload = [0u8; 16];
    trace_common(&mut payload[..8], 0);
    payload[8..].copy_from_slice(&dynamic_source_key(event).to_ne_bytes());
    emit_current_raw(event, &payload);
}

/// Deliver an already materialized trace payload without allocation.  The
/// record backend prepends the Linux PERF_SAMPLE_RAW length word and record
/// header; `raw` itself is the trace entry userspace decodes from tracefs.
pub(crate) fn emit_current_raw(event: PerfEvent, raw: &[u8]) {
    emit_current_raw_at(event, 0, raw);
}

/// Trap sources provide their architectural instruction pointer explicitly.
/// Tracepoint payloads are data, never a fabricated replacement for that IP.
pub(crate) fn emit_current_raw_at(event: PerfEvent, ip: u64, raw: &[u8]) {
    let current = axtask::current();
    let thread = current.try_as_thread();
    if let Some(thread) = thread {
        thread.perf_emit_dynamic_raw_at(event, ip, raw);
    } else {
        crate::file::PerfGroup::cpu_context_dynamic_raw_at(
            axhal::percpu::this_cpu_id(),
            event,
            ip,
            raw,
        );
    }
    emit_dynamic_tracepoints(thread, event, raw);
}

/// Route one trap payload to all dynamic tracepoint IDs naming its source.
/// The registry scan is bounded and allocation-free; name mutation uses the
/// source/key publication order above and path lookup remains non-cacheable.
fn emit_dynamic_tracepoints(thread: Option<&crate::task::Thread>, event: PerfEvent, raw: &[u8]) {
    let source = dynamic_source_key(event);
    if source == 0 {
        return;
    }
    for (index, registered) in DYNAMIC_TRACEPOINTS.iter().enumerate() {
        if registered.load(Ordering::Acquire) == 0
            || DYNAMIC_TRACEPOINT_ENABLED[index].load(Ordering::Acquire) == 0
            || DYNAMIC_TRACEPOINT_SOURCES[index].load(Ordering::Acquire) != source
        {
            continue;
        }
        let id = DYNAMIC_TRACEPOINT_BASE + index as u64;
        let mut entry = [0u8; 64];
        let copied = raw.len().min(entry.len());
        entry[..copied].copy_from_slice(&raw[..copied]);
        trace_common(&mut entry[..8], id as u16);
        if let Some(thread) = thread {
            thread.perf_emit_tracepoint_raw(id, &entry[..copied.max(8)], axhal::time::monotonic_time_nanos());
        } else {
            crate::file::PerfGroup::cpu_context_tracepoint(
                axhal::percpu::this_cpu_id(),
                id,
                &entry[..copied.max(8)],
                axhal::time::monotonic_time_nanos(),
            );
        }
    }
}

/// Bounded atomic-source key. It deliberately incorporates every uprobe
/// identity component and the return bit, unlike the old raw address-only
/// discriminator used by the direct PMU source backend.
fn dynamic_source_key(event: PerfEvent) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut add = |value: u64| {
        for byte in value.to_ne_bytes() {
            hash = (hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    match event {
        PerfEvent::Kprobe { addr, retprobe, .. } => {
            add(1);
            add(addr);
            add(retprobe as u64);
        }
        PerfEvent::Uprobe {
            mount_id,
            device,
            inode,
            offset,
            retprobe,
            ..
        } => {
            add(2);
            add(mount_id);
            add(device);
            add(inode);
            add(offset);
            add(retprobe as u64);
        }
        _ => return 0,
    }
    hash.max(1)
}

fn trace_common(out: &mut [u8], kind: u16) {
    // struct trace_entry { u16 type; u8 flags; u8 preempt_count; s32 pid; }
    out[..2].copy_from_slice(&kind.to_ne_bytes());
    out[2] = 0;
    out[3] = 0;
    out[4..8].copy_from_slice(&(axtask::current().id().as_u64() as i32).to_ne_bytes());
}

pub(crate) fn emit_raw_syscall_enter(
    number: u64,
    args: [u64; 6],
    regs: &axcpu::uspace::LinuxPtRegs,
) {
    // raw_syscalls:sys_enter: common header + long id + unsigned long args[6]
    let mut payload = [0u8; 64];
    trace_common(&mut payload, RAW_SYSCALLS_ENTER_TRACEPOINT_ID as u16);
    payload[8..16].copy_from_slice(&number.to_ne_bytes());
    for (index, arg) in args.into_iter().enumerate() {
        let offset = 16 + index * 8;
        payload[offset..offset + 8].copy_from_slice(&arg.to_ne_bytes());
    }
    #[cfg(feature = "bpf")]
    {
        // The raw prototype is exactly `(struct pt_regs *regs, long id)`.
        // `regs` is an invocation-local, read-only x86_64-style register
        // snapshot. The interpreter dereferences it only through the bounded
        // custom capability installed for this synchronous call.
        let mut regs_bytes = [0u8; axcpu::uspace::LinuxPtRegs::BYTE_LEN];
        regs.write_native_bytes(&mut regs_bytes);
        let mut raw_args = [0u8; 16];
        // The VM recognizes the 64-bit ctx[0] field as the bounded registers
        // capability. Keep its scalar bits zero so narrower scalar loads
        // cannot reconstruct an invocation-stack address.
        raw_args[8..16].copy_from_slice(&number.to_ne_bytes());
        crate::bpf::run_raw_tracepoint_links_with_regs(
            RAW_SYSCALLS_ENTER_TRACEPOINT_ID,
            &mut raw_args,
            Some(&regs_bytes),
        );
    }
    if let Some(thread) = axtask::current().try_as_thread() {
        thread.perf_emit_tracepoint_raw(RAW_SYSCALLS_ENTER_TRACEPOINT_ID, &payload, axhal::time::monotonic_time_nanos());
    }
}

pub(crate) fn emit_raw_syscall_exit(number: u64, result: i64, regs: &axcpu::uspace::LinuxPtRegs) {
    // raw_syscalls:sys_exit: common header + long id + long ret
    let mut payload = [0u8; 24];
    trace_common(&mut payload, RAW_SYSCALLS_EXIT_TRACEPOINT_ID as u16);
    payload[8..16].copy_from_slice(&number.to_ne_bytes());
    payload[16..24].copy_from_slice(&result.to_ne_bytes());
    #[cfg(feature = "bpf")]
    {
        // Prototype: `(struct pt_regs *regs, long ret)`.
        let mut regs_bytes = [0u8; axcpu::uspace::LinuxPtRegs::BYTE_LEN];
        regs.write_native_bytes(&mut regs_bytes);
        let mut raw_args = [0u8; 16];
        // See sys_enter: ctx[0] is capability-typed only for an exact u64
        // load and must never disclose the host snapshot address as a scalar.
        raw_args[8..].copy_from_slice(&result.to_ne_bytes());
        crate::bpf::run_raw_tracepoint_links_with_regs(
            RAW_SYSCALLS_EXIT_TRACEPOINT_ID,
            &mut raw_args,
            Some(&regs_bytes),
        );
    }
    if let Some(thread) = axtask::current().try_as_thread() {
        thread.perf_emit_tracepoint_raw(RAW_SYSCALLS_EXIT_TRACEPOINT_ID, &payload, axhal::time::monotonic_time_nanos());
    }
}

/// Emit one Linux-shaped `sched:sched_switch` entry at the exact scheduler
/// handoff. Both names are bounded snapshots supplied by the non-allocating
/// task adapter and both identities are the Linux thread IDs.
pub(crate) fn emit_sched_switch(
    publisher: Option<&crate::task::Thread>,
    previous_tid: u32,
    previous_name: &[u8],
    next_tid: u32,
    next_name: &[u8],
    previous_state: u64,
    timestamp: u64,
    previous_priority: i32,
    next_priority: i32,
) {
    let mut payload = [0u8; 64];
    payload[..2].copy_from_slice(&(SCHED_SWITCH_TRACEPOINT_ID as u16).to_ne_bytes());
    payload[4..8].copy_from_slice(&(previous_tid as i32).to_ne_bytes());
    // prev_comm[16], prev_pid, prev_prio, prev_state, next_comm[16], next_pid, next_prio
    let previous_name_len = previous_name.len().min(16);
    payload[8..8 + previous_name_len].copy_from_slice(&previous_name[..previous_name_len]);
    payload[24..28].copy_from_slice(&(previous_tid as i32).to_ne_bytes());
    payload[28..32].copy_from_slice(&previous_priority.to_ne_bytes());
    payload[32..40].copy_from_slice(&previous_state.to_ne_bytes());
    let next_name_len = next_name.len().min(16);
    payload[40..40 + next_name_len].copy_from_slice(&next_name[..next_name_len]);
    payload[56..60].copy_from_slice(&(next_tid as i32).to_ne_bytes());
    payload[60..64].copy_from_slice(&next_priority.to_ne_bytes());
    #[cfg(feature = "bpf")]
    {
        // Prototype slots are `(preempt, prev, next, prev_state)`.  Task
        // pointers are intentionally opaque to the bytecode engine, while
        // scalar state remains exact.
        let mut raw_args = [0u8; 32];
        raw_args[24..].copy_from_slice(&previous_state.to_ne_bytes());
        crate::bpf::run_raw_tracepoint_links(SCHED_SWITCH_TRACEPOINT_ID, &mut raw_args);
    }
    if let Some(publisher) = publisher {
        publisher.perf_emit_tracepoint_raw(SCHED_SWITCH_TRACEPOINT_ID, &payload, timestamp);
    } else {
        crate::file::PerfGroup::cpu_context_tracepoint(
            axhal::percpu::this_cpu_id(),
            SCHED_SWITCH_TRACEPOINT_ID,
            &payload,
            timestamp,
        );
    }
}

pub(crate) fn emit_sched_wakeup(
    publisher: Option<&crate::task::Thread>,
    waker_tid: u32,
    target_tid: u32,
    name: &[u8],
    target_cpu: usize,
    timestamp: u64,
    priority: i32,
) {
    let mut payload = [0u8; 36];
    payload[..2].copy_from_slice(&(SCHED_WAKEUP_TRACEPOINT_ID as u16).to_ne_bytes());
    payload[4..8].copy_from_slice(&(waker_tid as i32).to_ne_bytes());
    let name_len = name.len().min(16);
    payload[8..8 + name_len].copy_from_slice(&name[..name_len]);
    payload[24..28].copy_from_slice(&(target_tid as i32).to_ne_bytes());
    payload[28..32].copy_from_slice(&priority.to_ne_bytes());
    payload[32..36].copy_from_slice(&(target_cpu as i32).to_ne_bytes());
    if let Some(publisher) = publisher {
        publisher.perf_emit_tracepoint_raw(SCHED_WAKEUP_TRACEPOINT_ID, &payload, timestamp);
    } else {
        crate::file::PerfGroup::cpu_context_tracepoint(
            axhal::percpu::this_cpu_id(),
            SCHED_WAKEUP_TRACEPOINT_ID,
            &payload,
            timestamp,
        );
    }
}

#[register_trap_handler(DEBUG)]
fn perf_debug_exception(frame: &mut axcpu::TrapFrame) -> bool {
    let status = axcpu::asm::read_perf_debug_status();
    // BS and B0..B3 can be asserted together.  Claim only BS when a matching
    // XOL step completed; watchpoint status remains available below.
    let uprobe_handled = crate::uprobe::debug(frame, status);
    if uprobe_handled {
        axcpu::asm::acknowledge_perf_debug_status(1 << 14);
    }
    let kprobe_step = (!uprobe_handled && status & (1 << 14) != 0)
        .then(|| kprobe_finish_single_step(frame))
        .flatten();
    let kprobe_handled = kprobe_step == Some(false);
    if kprobe_handled {
        axcpu::asm::acknowledge_perf_debug_status(1 << 14);
    }
    let slots = status & 0x0f;
    let preserve_bs = status & (1 << 14) != 0 && !(uprobe_handled || kprobe_handled);
    if slots != 0 {
        let current = axtask::current();
        if let Some(thread) = current.try_as_thread() {
            thread.perf_emit_debug_exception(slots, frame.rip, frame.cs & 3 == 3);
        } else {
            let mut slot = 0;
            crate::file::PerfGroup::cpu_context_debug_exception(
                axhal::percpu::this_cpu_id(),
                slots,
                &mut slot,
                frame.rip,
                frame.cs & 3 == 3,
            );
        }
        axcpu::asm::acknowledge_perf_debug_status(slots);
        return !preserve_bs;
    }
    uprobe_handled || kprobe_handled
}

#[register_trap_handler(BREAKPOINT)]
fn perf_instruction_probe(frame: &mut axcpu::TrapFrame) -> bool {
    if frame.cs & 3 == 3
        && crate::uprobe::breakpoint(frame) == crate::uprobe::BreakpointClaim::Claimed
    {
        return true;
    }
    if claim_kretprobe_return(frame) {
        return true;
    }
    // INT3 reports the instruction following the patched byte. Probe
    // registration compares this exact address before claiming #BP, leaving
    // ordinary user breakpoints to the existing diagnostic/signal path.
    let address = frame.rip.wrapping_sub(1);
    if !kprobe_active(address) {
        return false;
    }
    let return_instance = prepare_kretprobe_instance(frame, address);
    if !kprobe_begin_single_step(frame, address) {
        rollback_kretprobe_instance(frame, return_instance);
        return false;
    }
    // `events/kprobes/<event>/format` describes a normal trace_entry followed
    // by the trapped instruction pointer.
    let mut payload = [0u8; 16];
    trace_common(&mut payload[..8], 0);
    payload[8..].copy_from_slice(&address.to_ne_bytes());
    emit_current_raw_at(
        PerfEvent::Kprobe {
            addr: address,
            retprobe: false,
            query_offset: 0,
        },
        frame.rip,
        &payload,
    );
    true
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;

    use super::{SCHED_SWITCH_TRACEPOINT_ID, tracefs_format, tracefs_id, tracepoint};

    #[test]
    fn tracefs_registry_has_stable_id_and_format() {
        assert_eq!(
            tracefs_id("sched", "sched_switch"),
            Some(SCHED_SWITCH_TRACEPOINT_ID)
        );
        assert!(
            tracefs_format("sched", "sched_switch")
                .unwrap()
                .contains("ID: 1")
        );
        assert_eq!(tracepoint(99), Err(AxError::OperationNotSupported));
        assert_eq!(tracefs_id("sched", "sched_wakeup"), Some(super::SCHED_WAKEUP_TRACEPOINT_ID));
        let wake = tracefs_format("sched", "sched_wakeup").unwrap();
        assert!(wake.contains("pid;\toffset:24;\tsize:4"));
        assert!(wake.contains("target_cpu;\toffset:32;\tsize:4"));
        assert!(super::raw_tracepoint(b"sched_wakeup").is_none());
        assert!(super::DYNAMIC_TRACEPOINT_BASE > super::SCHED_WAKEUP_TRACEPOINT_ID);
    }
}
