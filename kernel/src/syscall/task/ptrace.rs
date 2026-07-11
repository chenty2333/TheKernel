use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::MappingFlags;
use axtask::current;
use memory_addr::{MemoryAddr, VirtAddr};
use starry_process::Pid;
use starry_signal::{SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr};

use crate::task::{
    AsThread, ProcessData, PtraceCredentialMode, check_current_ptrace_access, get_process_data,
    get_task, notify_ptrace_attach_stop, reinject_ptrace_signal, send_signal_to_process,
};

const PTRACE_TRACEME: u32 = 0;
const PTRACE_PEEKTEXT: u32 = 1;
const PTRACE_PEEKDATA: u32 = 2;
const PTRACE_PEEKUSER: u32 = 3;
const PTRACE_POKETEXT: u32 = 4;
const PTRACE_POKEDATA: u32 = 5;
const PTRACE_POKEUSER: u32 = 6;
const PTRACE_CONT: u32 = 7;
const PTRACE_KILL: u32 = 8;
const PTRACE_SINGLESTEP: u32 = 9;
const PTRACE_ATTACH: u32 = 16;
const PTRACE_DETACH: u32 = 17;
const PTRACE_SYSCALL: u32 = 24;
const PTRACE_SETOPTIONS: u32 = 0x4200;
const PTRACE_GETEVENTMSG: u32 = 0x4201;
const PTRACE_GETSIGINFO: u32 = 0x4202;
const PTRACE_SETSIGINFO: u32 = 0x4203;
const PTRACE_GETREGSET: u32 = 0x4204;
const PTRACE_SETREGSET: u32 = 0x4205;
const PTRACE_SEIZE: u32 = 0x4206;
const PTRACE_INTERRUPT: u32 = 0x4207;
const PTRACE_LISTEN: u32 = 0x4208;

const PTRACE_O_MASK: usize = 0x2f_ffff;

fn ptrace_io_error() -> AxError {
    LinuxError::EIO.into()
}

fn current_pid() -> Pid {
    current().as_thread().proc_data.proc.pid()
}

fn check_ptrace_permission(target: &ProcessData) -> AxResult<()> {
    check_current_ptrace_access(target, PtraceCredentialMode::Real)
}

fn check_tracee(target: &ProcessData) -> AxResult<()> {
    if target.is_traced_by(current_pid()) {
        Ok(())
    } else {
        Err(AxError::NoSuchProcess)
    }
}

fn parse_signal(data: usize) -> AxResult<Option<SignalInfo>> {
    if data == 0 {
        return Ok(None);
    }
    let raw = u8::try_from(data).map_err(|_| AxError::InvalidInput)?;
    let signo = Signo::from_repr(raw).ok_or(AxError::InvalidInput)?;
    Ok(Some(SignalInfo::new_kernel(signo)))
}

fn interrupt_process_threads(target: &ProcessData) {
    for tid in target.proc.thread_ids() {
        if let Ok(task) = get_task(tid) {
            task.interrupt();
        }
    }
}

fn do_attach(target: &ProcessData, seize_only: bool) -> AxResult<isize> {
    let curr = current();
    let tracer_data = curr.as_thread().proc_data.clone();
    let tracer = tracer_data.proc.pid();
    if target.proc.pid() == tracer {
        return Err(AxError::OperationNotPermitted);
    }
    check_ptrace_permission(target)?;
    if !target.begin_ptrace(tracer) {
        return Err(AxError::OperationNotPermitted);
    }
    tracer_data.add_ptrace_tracee(target.proc.pid());
    if !seize_only && target.ptrace_stop(Signo::SIGSTOP as u8) {
        notify_ptrace_attach_stop(target);
        interrupt_process_threads(target);
    }
    Ok(0)
}

fn do_continue(target: &ProcessData, data: usize, detach: bool) -> AxResult<isize> {
    check_tracee(target)?;
    let curr = current();
    let tracer_data = curr.as_thread().proc_data.clone();
    let tracer = tracer_data.proc.pid();
    let signal = parse_signal(data)?.map(|info| info.signo());
    let (resume_result, record) = target
        .resume_ptrace(tracer, detach)
        .ok_or(AxError::NoSuchProcess)?;
    if detach {
        tracer_data.remove_ptrace_tracee(target.proc.pid());
    }
    let reinjected = reinject_ptrace_signal(target, record, signal);
    target.finish_ptrace_resume(resume_result);
    reinjected?;
    Ok(0)
}

fn validate_remote_access(
    target: &ProcessData,
    addr: usize,
    len: usize,
    flags: MappingFlags,
) -> AxResult<()> {
    let start = VirtAddr::from_usize(addr);
    let end = start.checked_add(len).ok_or_else(ptrace_io_error)?;
    let page_start = start.align_down_4k();
    let page_end = VirtAddr::from_usize(
        crate::mm::checked_align_up_4k(end.as_usize()).ok_or_else(ptrace_io_error)?,
    );
    let aspace_handle = target.aspace();
    let mut aspace = aspace_handle.lock();
    if !aspace.can_access_range(start, len, flags) {
        return Err(ptrace_io_error());
    }
    aspace
        .populate_area(page_start, page_end.sub_addr(page_start), flags)
        .map_err(|_| ptrace_io_error())
}

fn peek_word(target: &ProcessData, addr: usize) -> AxResult<isize> {
    check_tracee(target)?;
    let mut word = [0u8; size_of::<usize>()];
    validate_remote_access(target, addr, word.len(), MappingFlags::READ)?;
    target
        .aspace()
        .lock()
        .read(VirtAddr::from_usize(addr), &mut word)
        .map_err(|_| ptrace_io_error())?;
    Ok(usize::from_ne_bytes(word) as isize)
}

fn poke_word(target: &ProcessData, addr: usize, data: usize) -> AxResult<isize> {
    check_tracee(target)?;
    let word = data.to_ne_bytes();
    validate_remote_access(target, addr, word.len(), MappingFlags::WRITE)?;
    target
        .aspace()
        .lock()
        .write(VirtAddr::from_usize(addr), &word)
        .map_err(|_| ptrace_io_error())?;
    Ok(0)
}

fn sys_ptrace_traceme() -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let parent = proc_data
        .proc
        .parent()
        .ok_or(AxError::OperationNotPermitted)?;
    let parent_data = get_process_data(parent.pid()).map_err(|_| AxError::OperationNotPermitted)?;
    if proc_data.begin_ptrace(parent.pid()) {
        parent_data.add_ptrace_tracee(proc_data.proc.pid());
        Ok(0)
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn sys_ptrace_for_target(request: u32, pid: Pid, addr: usize, data: usize) -> AxResult<isize> {
    let target = get_process_data(pid)?;
    match request {
        PTRACE_ATTACH => do_attach(&target, false),
        PTRACE_SEIZE => {
            if data & !PTRACE_O_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            let result = do_attach(&target, true)?;
            target.ptrace_set_options(data as u32);
            Ok(result)
        }
        PTRACE_CONT | PTRACE_SYSCALL | PTRACE_SINGLESTEP => do_continue(&target, data, false),
        PTRACE_DETACH => do_continue(&target, data, true),
        PTRACE_KILL => {
            check_tracee(&target)?;
            send_signal_to_process(
                target.proc.pid(),
                Some(SignalInfo::new_kernel(Signo::SIGKILL)),
            )?;
            Ok(0)
        }
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => peek_word(&target, addr),
        PTRACE_POKETEXT | PTRACE_POKEDATA => poke_word(&target, addr, data),
        PTRACE_PEEKUSER | PTRACE_POKEUSER => {
            check_tracee(&target)?;
            Err(ptrace_io_error())
        }
        PTRACE_SETOPTIONS => {
            check_tracee(&target)?;
            if data & !PTRACE_O_MASK != 0 {
                return Err(AxError::InvalidInput);
            }
            target.ptrace_set_options(data as u32);
            Ok(0)
        }
        PTRACE_GETEVENTMSG => {
            check_tracee(&target)?;
            (data as *mut usize).vm_write(target.ptrace_event_message())?;
            Ok(0)
        }
        PTRACE_INTERRUPT => {
            check_tracee(&target)?;
            if target.ptrace_stop(Signo::SIGTRAP as u8) {
                notify_ptrace_attach_stop(&target);
                interrupt_process_threads(&target);
            }
            Ok(0)
        }
        PTRACE_LISTEN => do_continue(&target, 0, false),
        PTRACE_GETSIGINFO => {
            check_tracee(&target)?;
            let info = target.ptrace_signal_info().ok_or_else(ptrace_io_error)?;
            (data as *mut SignalInfo).vm_write(info)?;
            Ok(0)
        }
        PTRACE_SETSIGINFO => {
            check_tracee(&target)?;
            let info = unsafe { (data as *const SignalInfo).vm_read_uninit()?.assume_init() };
            let signo = info.try_signo().ok_or(AxError::InvalidInput)?;
            if target
                .ptrace_signal_info()
                .is_none_or(|current| current.signo() != signo)
            {
                return Err(AxError::InvalidInput);
            }
            if !target.replace_ptrace_signal_info(info) {
                return Err(ptrace_io_error());
            }
            Ok(0)
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => {
            check_tracee(&target)?;
            Err(ptrace_io_error())
        }
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_ptrace(request: u32, pid: i32, addr: usize, data: usize) -> AxResult<isize> {
    match request {
        PTRACE_TRACEME => sys_ptrace_traceme(),
        _ => {
            if pid <= 0 {
                return Err(AxError::NoSuchProcess);
            }
            sys_ptrace_for_target(request, pid as Pid, addr, data)
        }
    }
}
