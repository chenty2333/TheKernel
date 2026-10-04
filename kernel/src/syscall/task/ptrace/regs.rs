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

/// Produce the Linux standard user XSAVE view, not the signal-frame trailer.
/// Init components have architectural values even when XSAVE omitted them.
fn normalize_fp_image(image: &mut [u8], features: u64, xstate: bool) {
    if features != 0 {
        let present = u64::from_le_bytes(image[512..520].try_into().unwrap());
        if present & 1 == 0 {
            image[..24].fill(0);
            image[..2].copy_from_slice(&0x037fu16.to_le_bytes());
            image[32..160].fill(0);
        }
        if present & 6 == 0 {
            image[24..28].copy_from_slice(&0x1f80u32.to_le_bytes());
        }
        if present & 2 == 0 {
            image[160..416].fill(0);
        }
    }
    image[416..512].fill(0);
    if xstate {
        image[464..472].copy_from_slice(&features.to_le_bytes());
    }
}

pub(super) fn floating_register_transfer(
    memory: &UserMemoryCapability,
    thread: &Thread,
    write: bool,
    xstate: bool,
    address: usize,
    len: usize,
) -> AxResult<isize> {
    let layout = axhal::asm::xsave_layout().map_err(|_| ptrace_io_error())?;
    let required = if xstate { layout.xstate_size } else { 512 };
    if write && len != required {
        return Err(if xstate {
            AxError::BadAddress
        } else {
            AxError::InvalidInput
        });
    }
    if !write && len == 0 {
        return Ok(0);
    }
    let mut image = axtask::XsaveImage::new(layout).map_err(|_| AxError::NoMemory)?;
    {
        let snapshot = thread.ptrace_xsave.lock();
        let snapshot = snapshot.as_ref().ok_or(AxError::NoSuchProcess)?;
        image.as_mut_bytes().copy_from_slice(snapshot.as_bytes());
    }
    normalize_fp_image(image.as_mut_bytes(), layout.xfeatures, xstate);
    if !write {
        memory
            .write_bytes(address, &image.as_bytes()[..len])
            .map_err(map_usercopy_error)?;
        return Ok(0);
    }
    // SAFETY: the owned aligned image has at least `len` initialized bytes;
    // successful read initializes every transferred byte before inspection.
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(
            image
                .as_mut_bytes()
                .as_mut_ptr()
                .cast::<core::mem::MaybeUninit<u8>>(),
            len,
        )
    };
    memory
        .read_bytes(address, bytes)
        .map_err(map_usercopy_error)?;
    if !xstate && layout.xfeatures != 0 {
        let mut present = u64::from_le_bytes(image.as_bytes()[512..520].try_into().unwrap());
        present |= 3;
        image.as_mut_bytes()[512..520].copy_from_slice(&present.to_le_bytes());
    }
    if !axhal::asm::xsave_image_header_valid(layout, image.as_bytes())
        || !axhal::asm::xsave_image_mxcsr_valid(image.as_bytes())
    {
        return Err(AxError::InvalidInput);
    }
    // Software-reserved bytes are output metadata, not restore controls.
    image.as_mut_bytes()[416..512].fill(0);
    let retired = thread.ptrace_xsave.lock().replace(image);
    drop(retired);
    Ok(0)
}

#[cfg(test)]
mod fp_tests {
    use super::normalize_fp_image;
    #[test]
    fn fp_init_and_user_xstate_metadata_are_not_signal_trailers() {
        let mut bytes = [0xa5u8; 832];
        bytes[512..576].fill(0);
        normalize_fp_image(&mut bytes, 7, true);
        assert_eq!(&bytes[..2], &0x37fu16.to_le_bytes());
        assert_eq!(&bytes[24..28], &0x1f80u32.to_le_bytes());
        assert!(bytes[32..416].iter().all(|&byte| byte == 0));
        assert_eq!(&bytes[464..472], &7u64.to_le_bytes());
        assert!(bytes[416..464].iter().all(|&byte| byte == 0));
        assert!(bytes[472..512].iter().all(|&byte| byte == 0));
        normalize_fp_image(&mut bytes, 7, false);
        assert!(bytes[416..512].iter().all(|&byte| byte == 0));
    }
}
