//! Register requests, kept separate from ptrace relationship authorization.
use super::*;

pub(super) fn general_register_transfer(
    memory: &UserMemoryCapability,
    thread: &Thread,
    write: bool,
    address: usize,
    len: usize,
) -> AxResult<isize> {
    if len == 0 {
        return Ok(0);
    }
    let mut regs = thread
        .ptrace_registers
        .lock()
        .ok_or(AxError::NoSuchProcess)?;
    if write {
        // Do not hold a spin lock across faulting usercopy.
        // SAFETY: regs is an initialized native word array; len is a clamped prefix.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                regs.as_mut_ptr().cast::<core::mem::MaybeUninit<u8>>(),
                len,
            )
        };
        memory
            .read_bytes(address, bytes)
            .map_err(map_usercopy_error)?;
        let mut context =
            axhal::uspace::UserContext::new(0, axhal::mem::VirtAddr::from_usize(0), 0);
        let previous = thread
            .ptrace_registers
            .lock()
            .ok_or(AxError::NoSuchProcess)?;
        context.rflags = previous[18];
        crate::task::registers::apply_gregs(&mut context, &previous)?;
        crate::task::registers::apply_gregs(&mut context, &regs)?;
        // Keep the unmodifiable flags from the original stopped frame.
        crate::task::registers::fill_gregs(&context, regs[15], &mut regs);
        *thread.ptrace_registers.lock() = Some(regs);
    } else {
        // SAFETY: regs is an initialized native word array; len is a clamped prefix.
        let bytes = unsafe { core::slice::from_raw_parts(regs.as_ptr().cast::<u8>(), len) };
        memory
            .write_bytes(address, bytes)
            .map_err(map_usercopy_error)?;
    }
    Ok(0)
}

pub(super) fn ptrace_user_word(
    memory: &UserMemoryCapability,
    thread: &Thread,
    request: u32,
    addr: usize,
    data: usize,
) -> AxResult<isize> {
    // Linux x86-64 struct user is 912 bytes; debug registers begin at 848.
    if addr & 7 != 0 || addr >= 912 {
        return Err(ptrace_io_error());
    }
    if request == PTRACE_PEEKUSER {
        let value = if addr < 216 {
            thread
                .ptrace_registers
                .lock()
                .ok_or(AxError::NoSuchProcess)?[addr / 8]
        } else if addr == 896 {
            0xffff0ff0
        } else {
            0
        };
        memory
            .write_value(data as *mut u64, value)
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    if addr >= 216 {
        return Err(ptrace_io_error());
    }
    let mut regs = thread
        .ptrace_registers
        .lock()
        .ok_or(AxError::NoSuchProcess)?;
    let mut context = axhal::uspace::UserContext::new(0, axhal::mem::VirtAddr::from_usize(0), 0);
    context.rflags = regs[18];
    crate::task::registers::apply_gregs(&mut context, &regs)?;
    regs[addr / 8] = data as u64;
    crate::task::registers::apply_gregs(&mut context, &regs)?;
    crate::task::registers::fill_gregs(&context, regs[15], &mut regs);
    *thread.ptrace_registers.lock() = Some(regs);
    Ok(0)
}
