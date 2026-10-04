//! Shared Linux x86-64 general-register image for cores and ptrace.
//! Layout and admission facts: Linux 7.2.3 arch/x86/kernel/ptrace.c.

use axerrno::{AxResult, LinuxError};
use axhal::uspace::UserContext;

pub(crate) const NUM_GREGS: usize = 27;
pub(crate) type GeneralRegisters = [u64; NUM_GREGS];
const FLAG_MASK: u64 = 0x54dd5;

pub(crate) fn fill_gregs(uctx: &UserContext, orig_rax: u64, regs: &mut [u64; NUM_GREGS]) {
    let frame = uctx.linux_pt_regs(orig_rax);
    let gregs = [
        frame.r15,
        frame.r14,
        frame.r13,
        frame.r12,
        frame.bp,
        frame.bx,
        frame.r11,
        frame.r10,
        frame.r9,
        frame.r8,
        frame.ax,
        frame.cx,
        frame.dx,
        frame.si,
        frame.di,
        frame.orig_ax,
        frame.ip,
        frame.cs,
        frame.flags,
        frame.sp,
        frame.ss,
    ];
    regs[..gregs.len()].copy_from_slice(&gregs);
    regs[21] = uctx.fs_base;
    regs[22] = uctx.gs_base;
    // The saved context has no legacy segment selectors; keep those slots zero.
    regs[23..].fill(0);
}

/// Install a debugger image only after validating every privileged field.
/// Legacy selectors are not transported by the saved context yet; nonzero
/// values fail closed instead of issuing an unvalidated kernel segment load.
pub(crate) fn apply_gregs(uctx: &mut UserContext, regs: &GeneralRegisters) -> AxResult<()> {
    let mut r = *regs;
    for i in [17, 20, 23, 24, 25, 26] {
        r[i] = r[i] as u16 as u64;
        if (r[i] != 0 && r[i] & 3 != 3) || ([17, 20].contains(&i) && r[i] == 0) {
            return Err(LinuxError::EIO.into());
        }
    }
    if r[17] != uctx.cs
        || r[20] != uctx.ss
        || r[23..].iter().any(|&selector| selector != 0)
        || r[16] >= 1 << 47
        || r[19] >= 1 << 47
        || r[21] >= 1 << 47
        || r[22] >= 1 << 47
    {
        return Err(LinuxError::EIO.into());
    }
    uctx.r15 = r[0];
    uctx.r14 = r[1];
    uctx.r13 = r[2];
    uctx.r12 = r[3];
    uctx.rbp = r[4];
    uctx.rbx = r[5];
    uctx.r11 = r[6];
    uctx.r10 = r[7];
    uctx.r9 = r[8];
    uctx.r8 = r[9];
    uctx.rax = r[10];
    uctx.rcx = r[11];
    uctx.rdx = r[12];
    uctx.rsi = r[13];
    uctx.rdi = r[14];
    uctx.rip = r[16];
    uctx.rflags = (uctx.rflags & !FLAG_MASK) | (r[18] & FLAG_MASK);
    uctx.rsp = r[19];
    uctx.fs_base = r[21];
    uctx.gs_base = r[22];
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn general_register_layout_roundtrip_and_privilege_filter() {
        let mut ctx = UserContext::new(0x1234, axhal::mem::VirtAddr::from_usize(0x5678), 0);
        let mut regs = [0; NUM_GREGS];
        fill_gregs(&ctx, 42, &mut regs);
        assert_eq!(core::mem::size_of_val(&regs), 216);
        assert_eq!(&regs[15..17], &[42, 0x1234]);
        regs[10] = 99;
        regs[18] = !0;
        let flags = ctx.rflags;
        apply_gregs(&mut ctx, &regs).unwrap();
        assert_eq!(ctx.rax, 99);
        assert_eq!(ctx.rflags & !FLAG_MASK, flags & !FLAG_MASK);
        for (index, value) in [
            (17, 0),
            (20, 0),
            (16, 1 << 47),
            (19, !0),
            (21, 1 << 47),
            (22, !0),
            (23, 1),
        ] {
            let mut bad = regs;
            bad[index] = value;
            assert!(apply_gregs(&mut ctx, &bad).is_err());
            assert_eq!(ctx.rax, 99);
        }
    }
}
