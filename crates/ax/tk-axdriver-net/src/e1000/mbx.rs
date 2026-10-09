//! Intel e1000 PF/VF mailbox operations.
//!
//! Translated from FreeBSD `sys/dev/e1000/e1000_mbx.c` at
//! `c2b7fe4a9e94a0edba9dd2772874928b565c4f9e` (BSD-3-Clause).
//! Copyright (c) 2001-2020, Intel Corporation.

use axdriver_base::{DevError, DevResult};

use super::{api::E1000MacType, osdep::E1000RegisterIo, registers::*};

const ERR_MBX: DevError = DevError::Io;
const V2P_REQ: u32 = 0x01;
const V2P_ACK: u32 = 0x02;
const V2P_VFU: u32 = 0x04;
const V2P_PFSTS: u32 = 0x10;
const V2P_PFACK: u32 = 0x20;
const V2P_RSTI: u32 = 0x40;
const V2P_RSTD: u32 = 0x80;
const V2P_R2C: u32 = 0xb0;
const P2V_STS: u32 = 0x01;
const P2V_ACK: u32 = 0x02;
const P2V_PFU: u32 = 0x08;
const VF_MAILBOX_SIZE: u16 = 16;
const VF_MBX_INIT_DELAY_US: u32 = 500;
const VFREQ_BIT: u32 = 1;
const VFACK_BIT: u32 = 1 << 16;
const VF_MAILBOX_BASE: u32 = 0x00800;
const V2P_MAILBOX_BASE: u32 = 0x00c40;
const P2V_MAILBOX_BASE: u32 = 0x00c00;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MailboxStats {
    pub msgs_tx: u64,
    pub msgs_rx: u64,
    pub reqs: u64,
    pub acks: u64,
    pub rsts: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MailboxKind {
    Generic,
    Pf,
    Vf,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MailboxState {
    pub size: u16,
    pub timeout: u32,
    pub delay_us: u32,
    pub kind: MailboxKind,
    pub stats: MailboxStats,
    pub v2p_shadow: u32,
}
impl Default for MailboxState {
    fn default() -> Self {
        Self {
            size: 0,
            timeout: 0,
            delay_us: 0,
            kind: MailboxKind::Generic,
            stats: MailboxStats::default(),
            v2p_shadow: 0,
        }
    }
}

pub trait MailboxCallbacks {
    fn check_msg(&mut self, mailbox: u16) -> DevResult;
    fn check_ack(&mut self, mailbox: u16) -> DevResult;
    fn check_reset(&mut self, mailbox: u16) -> DevResult;
    fn read(&mut self, message: &mut [u32], mailbox: u16, unlock: bool) -> DevResult;
    fn write(&mut self, message: &[u32], mailbox: u16) -> DevResult;
    fn unlock(&mut self, mailbox: u16) -> DevResult;
}

/// upstream: e1000_mbx.c e1000_null_mbx_check_for_flag()
pub const fn null_mbx_check_for_flag() -> DevResult {
    Ok(())
}
/// upstream: e1000_mbx.c e1000_null_mbx_transact()
pub const fn null_mbx_transact() -> DevResult {
    Ok(())
}
/// upstream: e1000_mbx.c e1000_null_mbx_read()
pub const fn null_mbx_read() -> DevResult {
    Ok(())
}

/// upstream: e1000_mbx.c e1000_read_mbx()
pub fn read_mbx<O: MailboxCallbacks>(
    state: &MailboxState,
    ops: &mut O,
    message: &mut [u32],
    size: u16,
    mailbox: u16,
    unlock: bool,
) -> DevResult {
    let count = size.min(state.size) as usize;
    if count > message.len() {
        return Err(DevError::InvalidParam);
    }
    ops.read(&mut message[..count], mailbox, unlock)
}

/// upstream: e1000_mbx.c e1000_write_mbx()
pub fn write_mbx<O: MailboxCallbacks>(
    state: &MailboxState,
    ops: &mut O,
    message: &[u32],
    size: u16,
    mailbox: u16,
) -> DevResult {
    if size > state.size || usize::from(size) > message.len() {
        return Err(ERR_MBX);
    }
    ops.write(&message[..size as usize], mailbox)
}

/// upstream: e1000_mbx.c e1000_check_for_msg()
pub fn check_for_msg<O: MailboxCallbacks>(ops: &mut O, mailbox: u16) -> DevResult {
    ops.check_msg(mailbox)
}
/// upstream: e1000_mbx.c e1000_check_for_ack()
pub fn check_for_ack<O: MailboxCallbacks>(ops: &mut O, mailbox: u16) -> DevResult {
    ops.check_ack(mailbox)
}
/// upstream: e1000_mbx.c e1000_check_for_rst()
pub fn check_for_rst<O: MailboxCallbacks>(ops: &mut O, mailbox: u16) -> DevResult {
    ops.check_reset(mailbox)
}
/// upstream: e1000_mbx.c e1000_unlock_mbx()
pub fn unlock_mbx<O: MailboxCallbacks>(ops: &mut O, mailbox: u16) -> DevResult {
    ops.unlock(mailbox)
}

/// upstream: e1000_mbx.c e1000_poll_for_msg()
pub fn poll_for_msg<I: E1000RegisterIo, S: MailboxCallbacks>(
    io: &mut I,
    state: &mut MailboxState,
    ops: &mut S,
    mailbox: u16,
) -> DevResult {
    let mut countdown = state.timeout;
    if countdown == 0 {
        return Err(ERR_MBX);
    }
    while countdown != 0 && ops.check_msg(mailbox).is_err() {
        countdown -= 1;
        if countdown != 0 {
            io.delay_us(state.delay_us);
        }
    }
    if countdown == 0 {
        state.timeout = 0;
        Err(ERR_MBX)
    } else {
        Ok(())
    }
}

/// upstream: e1000_mbx.c e1000_poll_for_ack()
pub fn poll_for_ack<I: E1000RegisterIo, S: MailboxCallbacks>(
    io: &mut I,
    state: &mut MailboxState,
    ops: &mut S,
    mailbox: u16,
) -> DevResult {
    let mut countdown = state.timeout;
    if countdown == 0 {
        return Err(ERR_MBX);
    }
    while countdown != 0 && ops.check_ack(mailbox).is_err() {
        countdown -= 1;
        if countdown != 0 {
            io.delay_us(state.delay_us);
        }
    }
    if countdown == 0 {
        state.timeout = 0;
        Err(ERR_MBX)
    } else {
        Ok(())
    }
}

/// upstream: e1000_mbx.c e1000_read_posted_mbx()
pub fn read_posted_mbx<I: E1000RegisterIo, O: MailboxCallbacks>(
    io: &mut I,
    state: &mut MailboxState,
    ops: &mut O,
    message: &mut [u32],
    size: u16,
    mailbox: u16,
) -> DevResult {
    poll_for_msg(io, state, ops, mailbox)?;
    let count = usize::from(size.min(state.size)).min(message.len());
    ops.read(&mut message[..count], mailbox, true)
}

/// upstream: e1000_mbx.c e1000_write_posted_mbx()
pub fn write_posted_mbx<I: E1000RegisterIo, O: MailboxCallbacks>(
    io: &mut I,
    state: &mut MailboxState,
    ops: &mut O,
    message: &[u32],
    size: u16,
    mailbox: u16,
) -> DevResult {
    if state.timeout == 0 {
        return Err(ERR_MBX);
    }
    write_mbx(state, ops, message, size, mailbox)?;
    poll_for_ack(io, state, ops, mailbox)
}

/// upstream: e1000_mbx.c e1000_init_mbx_ops_generic()
pub fn init_mbx_ops_generic(state: &mut MailboxState) {
    *state = MailboxState {
        size: 0,
        timeout: 0,
        delay_us: 0,
        kind: MailboxKind::Generic,
        stats: MailboxStats::default(),
        v2p_shadow: 0,
    };
}

/// upstream: e1000_mbx.c e1000_read_v2p_mailbox()
pub fn read_v2p_mailbox<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
) -> DevResult<u32> {
    let raw = io.read_register(V2P_MAILBOX_BASE)?;
    let mailbox = raw | state.v2p_shadow;
    state.v2p_shadow |= raw & V2P_R2C;
    Ok(mailbox)
}

/// upstream: e1000_mbx.c e1000_check_for_bit_vf()
pub fn check_for_bit_vf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    mask: u32,
) -> DevResult {
    let mailbox = read_v2p_mailbox(io, state)?;
    if mailbox & mask == 0 {
        return Err(ERR_MBX);
    }
    state.v2p_shadow &= !mask;
    Ok(())
}

/// upstream: e1000_mbx.c e1000_check_for_msg_vf()
pub fn check_for_msg_vf<I: E1000RegisterIo>(io: &mut I, state: &mut MailboxState) -> DevResult {
    check_for_bit_vf(io, state, V2P_PFSTS).map(|_| state.stats.reqs += 1)
}
/// upstream: e1000_mbx.c e1000_check_for_ack_vf()
pub fn check_for_ack_vf<I: E1000RegisterIo>(io: &mut I, state: &mut MailboxState) -> DevResult {
    check_for_bit_vf(io, state, V2P_PFACK).map(|_| state.stats.acks += 1)
}
/// upstream: e1000_mbx.c e1000_check_for_rst_vf()
pub fn check_for_rst_vf<I: E1000RegisterIo>(io: &mut I, state: &mut MailboxState) -> DevResult {
    check_for_bit_vf(io, state, V2P_RSTI | V2P_RSTD).map(|_| state.stats.rsts += 1)
}

/// upstream: e1000_mbx.c e1000_obtain_mbx_lock_vf()
pub fn obtain_mbx_lock_vf<I: E1000RegisterIo>(io: &mut I, attempts: usize) -> DevResult {
    for attempt in 0..=attempts {
        io.write_register(V2P_MAILBOX_BASE, V2P_VFU)?;
        if io.read_register(V2P_MAILBOX_BASE)? & V2P_VFU != 0 {
            return Ok(());
        }
        if attempt != attempts {
            io.delay_us(1000);
        }
    }
    Err(ERR_MBX)
}

/// upstream: e1000_mbx.c e1000_write_mbx_vf()
pub fn write_mbx_vf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    message: &[u32],
    _mailbox: u16,
) -> DevResult {
    let count = message.len();
    if count > usize::from(state.size) {
        return Err(DevError::InvalidParam);
    }
    obtain_mbx_lock_vf(io, 10)?;
    let _ = check_for_msg_vf(io, state);
    let _ = check_for_ack_vf(io, state);
    for (index, value) in message[..count].iter().enumerate() {
        io.write_register(VF_MAILBOX_BASE + index as u32 * 4, *value)?;
    }
    state.stats.msgs_tx += 1;
    io.write_register(V2P_MAILBOX_BASE, V2P_REQ)
}

/// upstream: e1000_mbx.c e1000_read_mbx_vf()
pub fn read_mbx_vf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    message: &mut [u32],
    _mailbox: u16,
) -> DevResult {
    let count = message.len().min(usize::from(state.size));
    obtain_mbx_lock_vf(io, 10)?;
    for (index, slot) in message[..count].iter_mut().enumerate() {
        *slot = io.read_register(VF_MAILBOX_BASE + index as u32 * 4)?;
    }
    io.write_register(V2P_MAILBOX_BASE, V2P_ACK)?;
    state.stats.msgs_rx += 1;
    Ok(())
}

/// upstream: e1000_mbx.c e1000_init_mbx_params_vf()
pub fn init_mbx_params_vf(state: &mut MailboxState) {
    *state = MailboxState {
        size: VF_MAILBOX_SIZE,
        timeout: 0,
        delay_us: VF_MBX_INIT_DELAY_US,
        kind: MailboxKind::Vf,
        stats: MailboxStats::default(),
        v2p_shadow: 0,
    };
}

/// upstream: e1000_mbx.c e1000_check_for_bit_pf()
pub fn check_for_bit_pf<I: E1000RegisterIo>(io: &mut I, mask: u32) -> DevResult {
    let cause = io.read_register(E1000_MBVFICR)?;
    if cause & mask == 0 {
        return Err(ERR_MBX);
    }
    io.write_register(E1000_MBVFICR, mask)
}
/// upstream: e1000_mbx.c e1000_check_for_msg_pf()
pub fn check_for_msg_pf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    vf: u16,
) -> DevResult {
    check_for_bit_pf(io, VFREQ_BIT << vf).map(|_| state.stats.reqs += 1)
}
/// upstream: e1000_mbx.c e1000_check_for_ack_pf()
pub fn check_for_ack_pf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    vf: u16,
) -> DevResult {
    check_for_bit_pf(io, VFACK_BIT << vf).map(|_| state.stats.acks += 1)
}
/// upstream: e1000_mbx.c e1000_check_for_rst_pf()
pub fn check_for_rst_pf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    vf: u16,
) -> DevResult {
    let mask = 1u32 << vf;
    if io.read_register(E1000_VFLRE)? & mask == 0 {
        return Err(ERR_MBX);
    }
    io.write_register(E1000_VFLRE, mask)?;
    state.stats.rsts += 1;
    Ok(())
}

/// upstream: e1000_mbx.c e1000_obtain_mbx_lock_pf()
pub fn obtain_mbx_lock_pf<I: E1000RegisterIo>(io: &mut I, vf: u16) -> DevResult {
    let reg = P2V_MAILBOX_BASE + u32::from(vf) * 4;
    io.write_register(reg, P2V_PFU)?;
    if io.read_register(reg)? & P2V_PFU != 0 {
        Ok(())
    } else {
        Err(ERR_MBX)
    }
}

/// upstream: e1000_mbx.c e1000_release_mbx_lock_pf()
pub fn release_mbx_lock_pf<I: E1000RegisterIo>(io: &mut I, vf: u16) -> DevResult {
    let reg = P2V_MAILBOX_BASE + u32::from(vf) * 4;
    let value = io.read_register(reg)?;
    if value & P2V_PFU != 0 {
        io.write_register(reg, value & !P2V_PFU)?
    }
    Ok(())
}

/// upstream: e1000_mbx.c e1000_write_mbx_pf()
pub fn write_mbx_pf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    message: &[u32],
    vf: u16,
) -> DevResult {
    if message.len() > usize::from(state.size) {
        return Err(DevError::InvalidParam);
    }
    obtain_mbx_lock_pf(io, vf)?;
    if io.read_register(E1000_MBVFICR)? & (VFREQ_BIT << vf) != 0 {
        release_mbx_lock_pf(io, vf)?;
        return Err(ERR_MBX);
    }
    let _ = check_for_msg_pf(io, state, vf);
    let _ = check_for_ack_pf(io, state, vf);
    for (index, value) in message.iter().enumerate() {
        io.write_register(
            VF_MAILBOX_BASE + u32::from(vf) * 0x40 + index as u32 * 4,
            *value,
        )?;
    }
    state.stats.msgs_tx += 1;
    io.write_register(P2V_MAILBOX_BASE + u32::from(vf) * 4, P2V_STS)
}

/// upstream: e1000_mbx.c e1000_read_mbx_pf()
pub fn read_mbx_pf<I: E1000RegisterIo>(
    io: &mut I,
    state: &mut MailboxState,
    message: &mut [u32],
    vf: u16,
    unlock: bool,
) -> DevResult {
    let count = message.len().min(usize::from(state.size));
    obtain_mbx_lock_pf(io, vf)?;
    let _ = check_for_msg_pf(io, state, vf);
    for (index, slot) in message[..count].iter_mut().enumerate() {
        *slot = io.read_register(VF_MAILBOX_BASE + u32::from(vf) * 0x40 + index as u32 * 4)?;
    }
    io.write_register(
        P2V_MAILBOX_BASE + u32::from(vf) * 4,
        P2V_ACK | if unlock { 0 } else { P2V_PFU },
    )?;
    state.stats.msgs_rx += 1;
    Ok(())
}

/// upstream: e1000_mbx.c e1000_init_mbx_params_pf()
pub fn init_mbx_params_pf(state: &mut MailboxState, mac: E1000MacType) -> DevResult {
    if matches!(
        mac,
        E1000MacType::I82576 | E1000MacType::I350 | E1000MacType::I354
    ) {
        *state = MailboxState {
            size: VF_MAILBOX_SIZE,
            timeout: 0,
            delay_us: 0,
            kind: MailboxKind::Pf,
            stats: MailboxStats::default(),
            v2p_shadow: 0,
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Mock {
        regs: alloc::collections::BTreeMap<u32, u32>,
        delays: u32,
    }
    impl Default for Mock {
        fn default() -> Self {
            Self {
                regs: alloc::collections::BTreeMap::new(),
                delays: 0,
            }
        }
    }
    impl E1000RegisterIo for Mock {
        fn read_register(&mut self, r: u32) -> DevResult<u32> {
            Ok(*self.regs.get(&r).unwrap_or(&0))
        }
        fn write_register(&mut self, r: u32, v: u32) -> DevResult {
            self.regs.insert(r, v);
            Ok(())
        }
        fn delay_us(&mut self, u: u32) {
            self.delays += u;
        }
        fn invalid_tail_write(&mut self, _: &'static str) {}
    }
    #[test]
    fn vf_v2p_read_to_clear_cache_preserves_status() {
        let mut io = Mock::default();
        let mut state = MailboxState::default();
        io.regs.insert(V2P_MAILBOX_BASE, V2P_PFSTS | V2P_PFACK);
        assert!(check_for_msg_vf(&mut io, &mut state).is_ok());
        io.regs.insert(V2P_MAILBOX_BASE, 0);
        assert!(check_for_ack_vf(&mut io, &mut state).is_ok());
        assert_eq!(state.stats.reqs, 1);
        assert_eq!(state.stats.acks, 1);
    }
    #[test]
    fn posted_timeout_poison_is_source_compatible() {
        let mut state = MailboxState {
            timeout: 2,
            delay_us: 5,
            ..MailboxState::default()
        };
        struct Fail;
        impl MailboxCallbacks for Fail {
            fn check_msg(&mut self, _: u16) -> DevResult {
                Err(DevError::Io)
            }
            fn check_ack(&mut self, _: u16) -> DevResult {
                Err(DevError::Io)
            }
            fn check_reset(&mut self, _: u16) -> DevResult {
                Err(DevError::Io)
            }
            fn read(&mut self, _: &mut [u32], _: u16, _: bool) -> DevResult {
                Ok(())
            }
            fn write(&mut self, _: &[u32], _: u16) -> DevResult {
                Ok(())
            }
            fn unlock(&mut self, _: u16) -> DevResult {
                Ok(())
            }
        }
        assert!(poll_for_msg(&mut Mock::default(), &mut state, &mut Fail, 0).is_err());
        assert_eq!(state.timeout, 0);
    }
}
