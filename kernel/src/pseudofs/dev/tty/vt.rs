//! Linux virtual-terminal switching state and its fbcon presentation hook.

use alloc::{borrow::Cow, boxed::Box, sync::Arc, vec::Vec};
use core::{any::Any, task::Context};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, VfsResult};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::Mutex;
use axtask::{WaitQueue, current};
use kspin::SpinNoIrq;
use lazy_static::lazy_static;
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_signal::{SignalInfo, Signo};

use super::{
    NTtyDriver, TtyFile,
    ntty::new_virtual_tty,
    seat::{SeatHooks, SeatLifecycle, SeatOwner, SeatTarget},
};
use crate::{
    file::{FileLike, IoDst, IoSrc, IoctlContext, Kstat, OfdIoStatus},
    pseudofs::{DeviceOpen, DeviceOps},
    task::{
        AsThread, ProcStateHint, has_pending_syscall_signal, send_signal_to_process,
        with_proc_state_hint,
    },
};

const MAX_VTS: usize = 63;
const VT_AUTO: u8 = 0;
const VT_PROCESS: u8 = 1;
const VT_ACKACQ: usize = 2;
const KD_TEXT: i32 = 0;
const KD_GRAPHICS: i32 = 1;
const KB_101: u8 = 0x02;
const K_XLATE: i32 = 0x01;
const K_OFF: i32 = 0x04;
// These two commands are present in Linux's VT uapi but not yet exported by
// linux-raw-sys 0.12.
const VT_LOCKSWITCH_CMD: u32 = 0x560b;
const VT_UNLOCKSWITCH_CMD: u32 = 0x560c;
const VT_PROCESS_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(5);

fn read_vt_mode(context: &IoctlContext, address: usize) -> AxResult<[u8; 8]> {
    let mut bytes = [core::mem::MaybeUninit::uninit(); 8];
    context
        .user_memory()
        .read_bytes(address, &mut bytes)
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(core::array::from_fn(|index| {
        // SAFETY: `read_bytes` initializes every byte before it succeeds.
        unsafe { bytes[index].assume_init() }
    }))
}

fn vt_number_from_arg(arg: usize) -> AxResult<u16> {
    let number = u16::try_from(arg).map_err(|_| AxError::InvalidInput)?;
    VtManager::check_vt(number)?;
    Ok(number)
}

fn kd_mode_from_arg(arg: usize) -> AxResult<i32> {
    i32::try_from(arg).map_err(|_| AxError::InvalidInput)
}

fn require_vt_control(context: &IoctlContext, number: u16) -> AxResult<()> {
    use linux_raw_sys::general::CAP_SYS_TTY_CONFIG;

    let capable = context
        .caller_cred()
        .has_effective_capability(CAP_SYS_TTY_CONFIG);
    let console: Arc<dyn Any + Send + Sync> = VT_MANAGER.tty_for(number);
    let controls_console = context
        .caller_session()
        .terminal()
        .is_some_and(|tty| Arc::ptr_eq(&tty, &console));
    if can_control_vt(capable, controls_console) {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn require_tty_config(context: &IoctlContext) -> AxResult<()> {
    use linux_raw_sys::general::CAP_SYS_TTY_CONFIG;
    context
        .caller_cred()
        .has_effective_capability(CAP_SYS_TTY_CONFIG)
        .then_some(())
        .ok_or(AxError::OperationNotPermitted)
}

const fn can_control_vt(capable: bool, controls_console: bool) -> bool {
    capable || controls_console
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessMode {
    owner: Pid,
    waitv: u8,
    relsig: i16,
    acqsig: i16,
    frsig: i16,
}

#[derive(Clone)]
struct Vt {
    allocated: bool,
    open_count: u32,
    graphics: bool,
    graphics_owner: Option<Pid>,
    graphics_uid: Option<u32>,
    kb_mode: i32,
    process: Option<ProcessMode>,
    tty: Arc<NTtyDriver>,
    poll: Arc<PollSet>,
}

struct State {
    active: u16,
    pending: Option<PendingSwitch>,
    delivery_in_flight: Option<PendingSwitch>,
    next_switch_generation: u64,
    locked: bool,
    vts: Vec<Vt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwitchPhase {
    Release { owner: Pid },
    Acquire { owner: Pid },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingSwitch {
    target: u16,
    phase: SwitchPhase,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SwitchSignal {
    pid: Pid,
    signal: i16,
    pending: PendingSwitch,
}

/// The global VT switch arbiter. Signal delivery happens after dropping
/// `state`; text presentation deliberately takes it to serialize against VT
/// mode and active-console changes.
pub struct VtManager {
    /// Serializes process-mode replacement/exit with final validation and
    /// signal delivery.  No spin or VT presentation lock is held while the
    /// signal subsystem runs.
    delivery: Mutex<()>,
    /// Serializes physical input batches with active-VT transitions. Input is
    /// processed by the console reader task, not directly by an IRQ handler,
    /// so this must be sleepable: echo can take the presentation gate.
    route: Mutex<()>,
    state: SpinNoIrq<State>,
    // Lock order whenever both are needed is route -> presentation -> state.
    // Presentation-only paths use presentation -> state. This serializes slow
    // framebuffer work with transitions without keeping IRQs disabled.
    presentation: Mutex<()>,
    changed: WaitQueue,
    delivery_finished: WaitQueue,
    poll: Arc<PollSet>,
    /// The slow device/session transaction state.  It is intentionally
    /// separate from VT's spin state; hooks are always called after reading
    /// the active VT snapshot.
    seat: SeatLifecycle,
}

struct KernelSeatHooks;

impl SeatHooks for KernelSeatHooks {
    fn prepare_release(&mut self, _from: SeatTarget) -> AxResult<()> {
        // Close the KMS admission gate first.  It linearizes with atomic
        // enqueue, then error-completes queued submissions.
        crate::drm::suspend_primary_kms_for_seat();
        Ok(())
    }

    fn release(&mut self, _from: SeatTarget) -> AxResult<()> {
        // Input is paused before primary master is relinquished, so neither a
        // compositor nor a stale event FD can consume an event after release.
        crate::pseudofs::dev::event::pause_input_devices();
        if let SeatTarget::Graphics(owner) = _from
            && let Some(uid) = owner.uid
        {
            super::seat::revoke_published_nodes(uid);
        }
        super::super::fb::vt_graphics_changed(true);
        Ok(())
    }

    fn prepare_acquire(&mut self, target: SeatTarget) -> AxResult<()> {
        if let SeatTarget::Graphics(owner) = target {
            let uid = owner.uid.ok_or(AxError::PermissionDenied)?;
            // ACL success is a prerequisite for reopening KMS/input gates.
            super::seat::grant_published_nodes(uid)?;
        }
        // Master acquisition itself is independent of the KMS admission gate;
        // presentation waits until `acquire` has reopened that gate.
        Ok(())
    }

    fn acquire(&mut self, target: SeatTarget) -> AxResult<()> {
        // The graphics client gets master through its normal primary-node
        // SET_MASTER after this gate opens.  Render nodes remain independent.
        crate::drm::resume_primary_kms_for_seat();
        crate::pseudofs::dev::event::resume_input_devices();
        if matches!(target, SeatTarget::Fbcon) {
            super::super::fb::vt_graphics_changed(false);
            VT_MANAGER.present_active();
        }
        Ok(())
    }

    fn abort(&mut self, _target: SeatTarget) {
        // Every abort lands on the known-good text console.  The operations
        // are idempotent for owner exit, hot unplug and seatd restart.
        crate::drm::resume_primary_kms_for_seat();
        crate::pseudofs::dev::event::resume_input_devices();
        super::super::fb::vt_graphics_changed(false);
        if let SeatTarget::Graphics(owner) = _target
            && let Some(uid) = owner.uid
        {
            super::seat::revoke_published_nodes(uid);
        }
        VT_MANAGER.present_active();
    }
}

lazy_static! {
    pub static ref VT_MANAGER: Arc<VtManager> = Arc::new(VtManager::new());
}

impl VtManager {
    pub fn new() -> Self {
        let mut vts = Vec::with_capacity(MAX_VTS);
        for number in 1..=MAX_VTS as u16 {
            vts.push(Vt {
                allocated: false,
                open_count: 0,
                graphics: false,
                graphics_owner: None,
                graphics_uid: None,
                kb_mode: K_XLATE,
                process: None,
                tty: new_virtual_tty(number),
                poll: Arc::new(PollSet::new()),
            });
        }
        // Linux's boot console exists before userspace opens tty1.
        vts[0].allocated = true;
        Self {
            delivery: Mutex::new(()),
            route: Mutex::new(()),
            state: SpinNoIrq::new(State {
                active: 1,
                pending: None,
                delivery_in_flight: None,
                next_switch_generation: 0,
                locked: false,
                vts,
            }),
            presentation: Mutex::new(()),
            changed: WaitQueue::new(),
            delivery_finished: WaitQueue::new(),
            poll: Arc::new(PollSet::new()),
            seat: SeatLifecycle::new(),
        }
    }

    pub fn active(&self) -> u16 {
        self.state.lock().active
    }

    pub fn graphics(&self, number: u16) -> bool {
        self.state
            .lock()
            .vts
            .get(number.saturating_sub(1) as usize)
            .is_some_and(|vt| vt.graphics)
    }

    pub(crate) fn tty_for(&self, number: u16) -> Arc<NTtyDriver> {
        self.state.lock().vts[number as usize - 1].tty.clone()
    }

    /// Recovers the fixed VT number for a type-erased controlling tty.
    pub(crate) fn number_for_tty(&self, tty: &(dyn Any + Send + Sync)) -> Option<u16> {
        let tty = tty.downcast_ref::<NTtyDriver>()?;
        let state = self.state.lock();
        state
            .vts
            .iter()
            .enumerate()
            .find(|(_, vt)| core::ptr::eq(Arc::as_ptr(&vt.tty), tty))
            .map(|(index, _)| index as u16 + 1)
    }

    fn active_tty(&self) -> (Arc<NTtyDriver>, Arc<PollSet>) {
        let state = self.state.lock();
        let vt = &state.vts[state.active as usize - 1];
        (vt.tty.clone(), vt.poll.clone())
    }

    /// The physical console has one reader.  Route each input batch only to
    /// the active VT's independently-owned line discipline.
    pub(crate) fn route_active_input(&self, bytes: &[u8]) -> AxResult<()> {
        let _route = self.route.lock();
        let (tty, poll) = self.active_tty();
        tty.inject_input(bytes)?;
        poll.wake();
        self.poll.wake();
        Ok(())
    }

    /// Observes the selected VT after dropping the state spin lock.
    pub fn present_active(&self) {
        let _presentation = self.presentation.lock();
        let active = {
            let state = self.state.lock();
            state.active
        };
        self.with_text_active_locked(active, || super::fbcon::present_while_text_active(active));
    }

    /// Reconciles the selected VT with DRM/input ownership.  This snapshots
    /// VT state under its spin lock and runs the transaction only afterwards;
    /// no ACL, DRM, input, or fbcon operation can nest under VT locks.
    fn reconcile_seat(&self) {
        let target = {
            let state = self.state.lock();
            let vt = &state.vts[state.active as usize - 1];
            if vt.graphics {
                SeatTarget::Graphics(SeatOwner {
                    pid: vt.graphics_owner,
                    uid: vt.graphics_uid,
                    vt: state.active,
                })
            } else {
                SeatTarget::Fbcon
            }
        };
        let mut hooks = KernelSeatHooks;
        if self.seat.transition(target, &mut hooks).is_err() {
            self.seat.abort_current(&mut hooks);
        }
    }

    /// Serializes text rendering with active-console and KD mode changes.
    /// `f` must not hold fbcon state while entering this method.
    pub fn with_text_active<R>(&self, number: u16, f: impl FnOnce() -> R) -> Option<R> {
        let _presentation = self.presentation.lock();
        self.with_text_active_locked(number, f)
    }

    fn with_text_active_locked<R>(&self, number: u16, f: impl FnOnce() -> R) -> Option<R> {
        let state = self.state.lock();
        let index = number.checked_sub(1)? as usize;
        let drawable = state.active == number && !state.vts[index].graphics;
        drop(state);
        drawable.then(f)
    }

    fn check_vt(number: u16) -> AxResult<usize> {
        if number == 0 || number as usize > MAX_VTS {
            Err(AxError::InvalidInput)
        } else {
            Ok(number as usize - 1)
        }
    }

    /// Waits until a signal claim has finished.  Writers take `delivery`
    /// before touching `pending`, so a mode change which loses the claim race
    /// is linearized after that signal delivery rather than cancelling a
    /// signal already in flight.
    fn wait_for_delivery_finish(&self) -> AxResult<()> {
        self.delivery_finished
            .wait_until(|| self.state.lock().delivery_in_flight.is_none())
            .map_err(Into::into)
    }
    /// Makes `target` active and, where needed, starts its VT_PROCESS
    /// acquire handshake.  Callers hold `route`, so the transition cannot
    /// split a physical input batch.
    fn begin_pending_locked(
        state: &mut State,
        target: u16,
        phase: SwitchPhase,
        signal: i16,
    ) -> SwitchSignal {
        state.next_switch_generation = state.next_switch_generation.saturating_add(1);
        let pending = PendingSwitch {
            target,
            phase,
            generation: state.next_switch_generation,
        };
        let pid = match phase {
            SwitchPhase::Release { owner } | SwitchPhase::Acquire { owner } => owner,
        };
        state.pending = Some(pending);
        SwitchSignal {
            pid,
            signal,
            pending,
        }
    }

    fn complete_switch_locked(&self, state: &mut State, target: u16) -> Option<SwitchSignal> {
        state.active = target;
        self.changed.notify_all(false);
        self.poll.wake();
        if let Some(mode) = state.vts[target as usize - 1]
            .process
            .filter(|mode| mode.acqsig != 0)
        {
            Some(Self::begin_pending_locked(
                state,
                target,
                SwitchPhase::Acquire { owner: mode.owner },
                mode.acqsig,
            ))
        } else {
            state.pending = None;
            None
        }
    }

    fn activate(&self, target: u16) -> AxResult<Option<SwitchSignal>> {
        let target_index = Self::check_vt(target)?;
        loop {
            let _delivery = self.delivery.lock();
            if self.state.lock().delivery_in_flight.is_some() {
                drop(_delivery);
                self.wait_for_delivery_finish()?;
                continue;
            }
            let _route = self.route.lock();
            let _presentation = self.presentation.lock();
            let mut state = self.state.lock();
            if state.locked {
                return Err(AxError::PermissionDenied);
            }
            state.vts[target_index].allocated = true;
            if state.active == target {
                return Ok(None);
            }
            let active_index = state.active as usize - 1;
            if let Some(mode) = state.vts[active_index]
                .process
                .filter(|mode| mode.relsig != 0)
            {
                return Ok(Some(Self::begin_pending_locked(
                    &mut state,
                    target,
                    SwitchPhase::Release { owner: mode.owner },
                    mode.relsig,
                )));
            }
            return Ok(self.complete_switch_locked(&mut state, target));
        }
    }
    fn release_reply(&self, caller: Pid, reply: usize) -> AxResult<Option<SwitchSignal>> {
        loop {
            let _delivery = self.delivery.lock();
            if self.state.lock().delivery_in_flight.is_some() {
                drop(_delivery);
                self.wait_for_delivery_finish()?;
                continue;
            }
            let _route = self.route.lock();
            let _presentation = self.presentation.lock();
            let mut state = self.state.lock();
            let pending = state.pending.ok_or(AxError::InvalidInput)?;
            match pending.phase {
                SwitchPhase::Release { owner } => {
                    if owner != caller || reply == VT_ACKACQ {
                        return Err(AxError::PermissionDenied);
                    }
                    if reply == 0 {
                        state.pending = None;
                        return Ok(None);
                    }
                    if reply != 1 {
                        return Err(AxError::InvalidInput);
                    }
                    return Ok(self.complete_switch_locked(&mut state, pending.target));
                }
                SwitchPhase::Acquire { owner } => {
                    if owner != caller || reply != VT_ACKACQ {
                        return Err(AxError::PermissionDenied);
                    }
                    state.pending = None;
                    return Ok(None);
                }
            }
        }
    }
    fn mode(&self, number: u16, owner: Pid, bytes: [u8; 8]) -> AxResult<()> {
        let index = Self::check_vt(number)?;
        let mode = bytes[0];
        if mode != VT_AUTO && mode != VT_PROCESS {
            return Err(AxError::InvalidInput);
        }
        let read_i16 = |at| i16::from_ne_bytes([bytes[at], bytes[at + 1]]);
        if mode == VT_PROCESS {
            for signal in [read_i16(2), read_i16(4), read_i16(6)] {
                let valid = signal == 0
                    || u8::try_from(signal)
                        .ok()
                        .and_then(Signo::from_repr)
                        .is_some();
                if !valid {
                    return Err(AxError::InvalidInput);
                }
            }
        }
        let process = (mode == VT_PROCESS).then_some(ProcessMode {
            owner,
            waitv: bytes[1],
            relsig: read_i16(2),
            acqsig: read_i16(4),
            frsig: read_i16(6),
        });
        loop {
            // Take the delivery gate before state so an already-emitted ticket
            // is either delivered while this exact mode is live, or rejected
            // after this replacement cancels it.
            let _delivery = self.delivery.lock();
            if self.state.lock().delivery_in_flight.is_some() {
                drop(_delivery);
                self.wait_for_delivery_finish()?;
                continue;
            }
            let mut state = self.state.lock();
            let previous = state.vts[index].process;
            state.vts[index].allocated = true;
            state.vts[index].process = process;
            // A VT_PROCESS handshake is owned by the mode instance which started
            // it, not merely by its PID.  Replacing that instance (including with
            // another mode for the same process) must make a late VT_RELDISP from
            // the old instance unable to complete the switch.
            let stale_pending = previous.is_some_and(|previous| {
                state.pending.is_some_and(|pending| match pending.phase {
                    SwitchPhase::Release {
                        owner: pending_owner,
                    } => state.active == number && pending_owner == previous.owner,
                    SwitchPhase::Acquire {
                        owner: pending_owner,
                    } => pending.target == number && pending_owner == previous.owner,
                })
            });
            if stale_pending {
                state.pending = None;
                self.changed.notify_all(false);
                self.poll.wake();
            }
            return Ok(());
        }
    }
    fn state_bytes(&self) -> [u8; 6] {
        let state = self.state.lock();
        // Linux's `v_state` is a 16-bit console bitmap: tty0 occupies bit
        // zero and tty1..tty15 occupy bits one through fifteen.
        let mut bits = 1u16;
        for number in 1..u16::BITS as u16 {
            let vt = &state.vts[number as usize - 1];
            if Self::in_use(&state, number, vt) {
                bits |= 1 << number;
            }
        }
        let mut out = [0; 6];
        out[..2].copy_from_slice(&state.active.to_ne_bytes());
        out[2..4].copy_from_slice(&0u16.to_ne_bytes());
        out[4..].copy_from_slice(&bits.to_ne_bytes());
        out
    }
    fn open_query(&self) -> u16 {
        let state = self.state.lock();
        state
            .vts
            .iter()
            .enumerate()
            .find(|(index, vt)| !Self::in_use(&state, *index as u16 + 1, vt))
            .map_or(0, |(index, _)| index as u16 + 1)
    }
    fn disallocate(&self, number: u16) -> AxResult<()> {
        if number == 0 {
            let mut state = self.state.lock();
            let active = state.active as usize;
            if state.vts.iter().enumerate().any(|(index, vt)| {
                index + 1 != active && Self::in_use(&state, index as u16 + 1, vt)
            }) {
                return Err(AxError::ResourceBusy);
            }
            for (index, vt) in state.vts.iter_mut().enumerate() {
                if index + 1 != active {
                    *vt = Vt {
                        allocated: false,
                        open_count: 0,
                        graphics: false,
                        graphics_owner: None,
                        graphics_uid: None,
                        kb_mode: K_XLATE,
                        process: None,
                        tty: vt.tty.clone(),
                        poll: vt.poll.clone(),
                    };
                }
            }
            return Ok(());
        }
        let index = Self::check_vt(number)?;
        let mut state = self.state.lock();
        if Self::in_use(&state, number, &state.vts[index]) {
            return Err(AxError::ResourceBusy);
        }
        state.vts[index].allocated = false;
        state.vts[index].process = None;
        state.vts[index].graphics = false;
        state.vts[index].graphics_owner = None;
        state.vts[index].graphics_uid = None;
        state.vts[index].kb_mode = K_XLATE;
        Ok(())
    }

    fn in_use(state: &State, number: u16, vt: &Vt) -> bool {
        state.active == number
            || vt.open_count != 0
            || vt.process.is_some()
            || vt.tty.has_controlling_session()
    }

    fn opened(&self, number: u16) -> AxResult<()> {
        let index = Self::check_vt(number)?;
        let mut state = self.state.lock();
        state.vts[index].allocated = true;
        state.vts[index].open_count = state.vts[index]
            .open_count
            .checked_add(1)
            .ok_or(AxError::NoMemory)?;
        Ok(())
    }

    fn closed(&self, number: u16) {
        let mut state = self.state.lock();
        let vt = &mut state.vts[number as usize - 1];
        vt.open_count = vt
            .open_count
            .checked_sub(1)
            .expect("VT open count underflow");
    }

    /// Retires a VT_PROCESS owner during process exit.  The process-exit path
    /// calls this without holding task/session locks; any acquire signal is
    /// delivered only after this method has dropped the VT lock.
    fn owner_exited(&self, pid: Pid) -> Option<SwitchSignal> {
        loop {
            let _delivery = self.delivery.lock();
            if self.state.lock().delivery_in_flight.is_some() {
                drop(_delivery);
                if self.wait_for_delivery_finish().is_err() {
                    // Exit cleanup cannot return an interrupted wait to a
                    // syscall caller. Reacquire the delivery gate and retry
                    // so it never races a still-running signal delivery.
                    continue;
                }
                continue;
            }
            return self.owner_exited_locked(pid);
        }
    }

    fn owner_exited_locked(&self, pid: Pid) -> Option<SwitchSignal> {
        let _route = self.route.lock();
        let _presentation = self.presentation.lock();
        let mut state = self.state.lock();
        let mut removed_owner = false;
        for vt in &mut state.vts {
            if vt.process.is_some_and(|mode| mode.owner == pid) || vt.graphics_owner == Some(pid) {
                vt.process = None;
                vt.graphics = false;
                vt.graphics_owner = None;
                vt.graphics_uid = None;
                vt.kb_mode = K_XLATE;
                removed_owner = true;
            }
        }
        if removed_owner {
            self.changed.notify_all(false);
            self.poll.wake();
        }
        match state.pending {
            Some(PendingSwitch {
                phase: SwitchPhase::Release { owner },
                target,
                ..
            }) if owner == pid => {
                state.pending = None;
                self.complete_switch_locked(&mut state, target)
            }
            Some(PendingSwitch {
                phase: SwitchPhase::Acquire { owner },
                ..
            }) if owner == pid => {
                state.pending = None;
                None
            }
            _ => None,
        }
    }

    /// Delivers one pending VT_PROCESS signal at a time.  Failed delivery is
    /// equivalent to owner exit; that recovery can complete a release and
    /// produce an acquire signal, which is delivered by the next loop turn.
    /// No signal is sent while any VT lock is held.
    fn deliver_switch_signal(&self, mut next: Option<SwitchSignal>) {
        while let Some(ticket) = next {
            let claimed = {
                let _delivery = self.delivery.lock();
                let mut state = self.state.lock();
                if state.pending != Some(ticket.pending) || state.delivery_in_flight.is_some() {
                    false
                } else {
                    // Claim is the delivery linearization point. A concurrent
                    // VT_SETMODE waits for this claim to finish, so it cannot
                    // cancel a signal after that point while no signal/task
                    // lock is retained by the sender.
                    state.delivery_in_flight = Some(ticket.pending);
                    true
                }
            };
            if !claimed {
                return;
            }
            // Never hold `delivery`, route, presentation, or state while
            // entering task/signal code.
            let delivered = Signo::from_repr(ticket.signal as u8).is_some_and(|signo| {
                send_signal_to_process(ticket.pid, Some(SignalInfo::new_kernel(signo))).is_ok()
            });
            {
                let _delivery = self.delivery.lock();
                let mut state = self.state.lock();
                if state.delivery_in_flight == Some(ticket.pending) {
                    state.delivery_in_flight = None;
                    self.delivery_finished.notify_all(false);
                }
            }
            if delivered {
                self.arm_process_timeout(ticket);
                return;
            }
            next = self.failed_switch_signal(ticket);
        }
    }

    /// A VT_PROCESS owner which neither accepts nor refuses its release is no
    /// longer allowed to hold the physical seat forever.  The timer captures
    /// the full generation ticket, so a late timeout cannot tear down a
    /// replacement compositor or a completed acquire.
    fn arm_process_timeout(&self, ticket: SwitchSignal) {
        if !core::ptr::eq(self, VT_MANAGER.as_ref()) {
            return;
        }
        let _ = axtask::try_spawn_with_name(
            move || {
                let _ = axtask::future::block_on(axtask::future::sleep(VT_PROCESS_TIMEOUT));
                if VT_MANAGER.ticket_is_current(ticket) {
                    let next = VT_MANAGER.owner_exited(ticket.pid);
                    VT_MANAGER.deliver_switch_signal(next);
                    VT_MANAGER.reconcile_seat();
                    VT_MANAGER.present_active();
                }
            },
            "vt-process-timeout".into(),
        );
    }

    fn failed_switch_signal(&self, ticket: SwitchSignal) -> Option<SwitchSignal> {
        let _delivery = self.delivery.lock();
        if self.state.lock().pending != Some(ticket.pending) {
            return None;
        }
        self.owner_exited_locked(ticket.pid)
    }

    fn ticket_is_current(&self, ticket: SwitchSignal) -> bool {
        let _delivery = self.delivery.lock();
        let state = self.state.lock();
        state.pending == Some(ticket.pending) && state.delivery_in_flight.is_none()
    }

    #[cfg(test)]
    fn claim_ticket_for_test(&self, ticket: SwitchSignal) -> bool {
        let _delivery = self.delivery.lock();
        let mut state = self.state.lock();
        if state.pending != Some(ticket.pending) || state.delivery_in_flight.is_some() {
            return false;
        }
        state.delivery_in_flight = Some(ticket.pending);
        true
    }

    #[cfg(test)]
    fn finish_ticket_for_test(&self, ticket: SwitchSignal) {
        let _delivery = self.delivery.lock();
        let mut state = self.state.lock();
        assert_eq!(state.delivery_in_flight, Some(ticket.pending));
        state.delivery_in_flight = None;
        self.delivery_finished.notify_all(false);
    }
}

/// Process-exit integration point.  It intentionally does not require a TTY
/// or session reference, avoiding exit-path lock nesting.
pub fn notify_vt_owner_exit(pid: Pid) {
    VT_MANAGER.deliver_switch_signal(VT_MANAGER.owner_exited(pid));
    VT_MANAGER.reconcile_seat();
    VT_MANAGER.present_active();
}

/// Pseudo-device endpoint.  Number zero is tty0, the active-console alias.
pub struct VtDevice {
    number: u16,
}

struct VtOpenGuard {
    number: Option<u16>,
}

impl Drop for VtOpenGuard {
    fn drop(&mut self) {
        if let Some(number) = self.number {
            VT_MANAGER.closed(number);
        }
    }
}

/// OFD transport for a VT.  The node object remains shared, while this holds
/// the one reference which makes an open console unavailable to disallocate.
struct VtFile {
    number: u16,
    tty_file: Arc<TtyFile<super::ntty::Console, super::ntty::Console>>,
    poll: Arc<PollSet>,
}

impl VtDevice {
    fn open_description(&self, location: &Location) -> AxResult<DeviceOpen> {
        let number = self.selected();
        VT_MANAGER.opened(number)?;
        let tty = VT_MANAGER.tty_for(number);
        let tty_file = match TtyFile::try_new(tty, location.clone()) {
            Ok(file) => file,
            Err(error) => {
                VT_MANAGER.closed(number);
                return Err(error);
            }
        };
        let file: Arc<dyn FileLike> = match Arc::try_new(VtFile {
            number,
            tty_file,
            poll: VT_MANAGER.state.lock().vts[number as usize - 1]
                .poll
                .clone(),
        }) {
            Ok(file) => file,
            Err(_) => {
                VT_MANAGER.closed(number);
                return Err(AxError::NoMemory);
            }
        };
        Ok(DeviceOpen::new(
            file,
            Some(Box::new(VtOpenGuard {
                number: Some(number),
            })),
        ))
    }
}

impl FileLike for VtFile {
    fn final_close(&self) {
        self.tty_file.final_close();
    }

    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.tty_file.read(dst)
    }

    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.tty_file.write(src)
    }

    fn read_with_operation_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        self.tty_file.read_with_operation_status(status, dst)
    }

    fn write_with_operation_status(&self, status: OfdIoStatus, src: &mut IoSrc) -> AxResult<usize> {
        self.tty_file.write_with_operation_status(status, src)
    }

    fn stat(&self) -> AxResult<Kstat> {
        self.tty_file.stat()
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        self.tty_file.path()
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        VtDevice::new(self.number).ioctl(context, cmd, arg)
    }

    fn nonblocking(&self) -> bool {
        self.tty_file.nonblocking()
    }

    fn set_nonblocking(&self, value: bool) -> AxResult<()> {
        self.tty_file.set_nonblocking(value)
    }
}

impl Pollable for VtFile {
    fn poll(&self) -> IoEvents {
        self.tty_file.poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        // The VT input route wakes `poll`, while the per-terminal N_TTY
        // registration includes its line-discipline and job-control sources.
        // Both are required: otherwise TIOCSPGRP can leave a blocked read or
        // epoll waiter asleep until unrelated keyboard input arrives.
        let mut prepared = axpoll::PreparedPollRegistration::try_new(2)?;
        prepared.arm_owned(self.poll.clone(), context.waker())?;
        prepared.arm_nested(|| self.tty_file.register(context, events))?;
        prepared.commit()
    }
}
impl VtDevice {
    pub fn new(number: u16) -> Self {
        Self { number }
    }

    pub(crate) const fn is_active_alias(&self) -> bool {
        self.number == 0
    }

    fn selected(&self) -> u16 {
        if self.number == 0 {
            VT_MANAGER.active()
        } else {
            self.number
        }
    }
}

impl Pollable for VtDevice {
    fn poll(&self) -> IoEvents {
        VT_MANAGER.tty_for(self.selected()).poll()
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::single_owned(VT_MANAGER.poll.clone(), context.waker())
    }
}

impl DeviceOps for VtDevice {
    fn open_description(&self, location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        Ok(Some(self.open_description(location)?))
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> AxResult<usize> {
        DeviceOps::read_at(VT_MANAGER.tty_for(self.selected()).as_ref(), buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> AxResult<usize> {
        let number = self.selected();
        // VT endpoints share the physical console's serial output, but must
        // not route through N_TTY's `/dev/console` fbcon mirror: an inactive
        // ttyN write belongs to ttyN, not the currently selected VT.
        let _ = offset;
        axhal::console::write_bytes(buf);
        super::fbcon::write(
            number,
            buf,
            VT_MANAGER.active(),
            VT_MANAGER.graphics(number),
        );
        Ok(buf.len())
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        use linux_raw_sys::ioctl::*;
        let number = self.selected();
        let owner = context.caller_process().proc.pid();
        match cmd {
            VT_OPENQRY => context
                .user_memory()
                .write_bytes(arg, &(VT_MANAGER.open_query() as i32).to_ne_bytes())
                .map_err(crate::mm::map_usercopy_error)?,
            VT_GETMODE => {
                let p = VT_MANAGER.state.lock().vts[VtManager::check_vt(number)?].process;
                let mut out = [0u8; 8];
                if let Some(p) = p {
                    out[0] = VT_PROCESS;
                    out[1] = p.waitv;
                    out[2..4].copy_from_slice(&p.relsig.to_ne_bytes());
                    out[4..6].copy_from_slice(&p.acqsig.to_ne_bytes());
                    out[6..8].copy_from_slice(&p.frsig.to_ne_bytes());
                }
                context
                    .user_memory()
                    .write_bytes(arg, &out)
                    .map_err(crate::mm::map_usercopy_error)?;
            }
            VT_SETMODE => {
                require_vt_control(context, number)?;
                VT_MANAGER.mode(number, owner, read_vt_mode(context, arg)?)?;
            }
            VT_GETSTATE => context
                .user_memory()
                .write_bytes(arg, &VT_MANAGER.state_bytes())
                .map_err(crate::mm::map_usercopy_error)?,
            VT_ACTIVATE => {
                require_vt_control(context, number)?;
                let signal = VT_MANAGER.activate(vt_number_from_arg(arg)?)?;
                VT_MANAGER.deliver_switch_signal(signal);
                VT_MANAGER.reconcile_seat();
                VT_MANAGER.present_active();
            }
            VT_RELDISP => {
                require_vt_control(context, number)?;
                let signal = VT_MANAGER.release_reply(owner, arg)?;
                VT_MANAGER.deliver_switch_signal(signal);
                VT_MANAGER.reconcile_seat();
                VT_MANAGER.present_active();
            }
            VT_WAITACTIVE => {
                require_vt_control(context, number)?;
                let want = vt_number_from_arg(arg)?;
                Self::wait_active(want)?;
            }
            VT_DISALLOCATE => {
                require_vt_control(context, number)?;
                let target = u16::try_from(arg).map_err(|_| AxError::InvalidInput)?;
                if target != 0 {
                    VtManager::check_vt(target)?;
                }
                VT_MANAGER.disallocate(target)?;
            }
            VT_LOCKSWITCH_CMD => {
                require_tty_config(context)?;
                VT_MANAGER.state.lock().locked = true;
            }
            VT_UNLOCKSWITCH_CMD => {
                require_tty_config(context)?;
                VT_MANAGER.state.lock().locked = false;
            }
            KDGETMODE => {
                let value = if VT_MANAGER.state.lock().vts[VtManager::check_vt(number)?].graphics {
                    KD_GRAPHICS
                } else {
                    KD_TEXT
                };
                context
                    .user_memory()
                    .write_bytes(arg, &value.to_ne_bytes())
                    .map_err(crate::mm::map_usercopy_error)?;
            }
            KDSETMODE => {
                require_vt_control(context, number)?;
                let mode = kd_mode_from_arg(arg)?;
                if mode != KD_TEXT && mode != KD_GRAPHICS {
                    return Err(AxError::InvalidInput);
                }
                {
                    let _presentation = VT_MANAGER.presentation.lock();
                    let vt = &mut VT_MANAGER.state.lock().vts[VtManager::check_vt(number)?];
                    vt.graphics = mode == KD_GRAPHICS;
                    vt.graphics_owner = (mode == KD_GRAPHICS).then_some(owner);
                    vt.graphics_uid =
                        (mode == KD_GRAPHICS).then(|| context.caller_cred().ids().euid.into_raw());
                }
                VT_MANAGER.reconcile_seat();
                VT_MANAGER.present_active();
            }
            KDGKBTYPE => context
                .user_memory()
                .write_bytes(arg, &[KB_101])
                .map_err(crate::mm::map_usercopy_error)?,
            KDGKBMODE => {
                let mode = VT_MANAGER.state.lock().vts[VtManager::check_vt(number)?].kb_mode;
                context
                    .user_memory()
                    .write_bytes(arg, &mode.to_ne_bytes())
                    .map_err(crate::mm::map_usercopy_error)?;
            }
            KDSKBMODE => {
                require_vt_control(context, number)?;
                let mode = kd_mode_from_arg(arg)?;
                if !(0..=K_OFF).contains(&mode) {
                    return Err(AxError::InvalidInput);
                }
                VT_MANAGER.state.lock().vts[VtManager::check_vt(number)?].kb_mode = mode
            }
            _ => return DeviceOps::ioctl(VT_MANAGER.tty_for(number).as_ref(), context, cmd, arg),
        };
        Ok(0)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_pollable(&self) -> Option<&dyn Pollable> {
        Some(self)
    }
}
impl VtDevice {
    fn wait_active(want: u16) -> AxResult<()> {
        VtManager::check_vt(want)?;
        let task = current();
        let thread = task.as_thread();
        if VT_MANAGER.active() == want {
            return Ok(());
        }
        if has_pending_syscall_signal(thread) {
            return Err(AxError::Interrupted);
        }
        with_proc_state_hint(ProcStateHint::Interruptible, || {
            VT_MANAGER.changed.wait_until_interruptible(|| {
                VT_MANAGER.active() == want || has_pending_syscall_signal(thread)
            })
        })
        .map_err(AxError::from)?;
        if has_pending_syscall_signal(thread) {
            return Err(AxError::Interrupted);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_switch_signal(signal: Option<SwitchSignal>, expected: (Pid, i16)) {
        let signal = signal.expect("expected a VT_PROCESS signal");
        assert_eq!((signal.pid, signal.signal), expected);
    }

    #[test]
    fn process_release_is_a_two_phase_switch() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 2, 0, 0, 0]).unwrap();
        assert_switch_signal(m.activate(2).unwrap(), (7, 1));
        assert_eq!(m.active(), 1);
        assert_eq!(m.release_reply(7, 1).unwrap(), None);
        assert_eq!(m.active(), 2);
    }
    #[test]
    fn process_mode_without_a_release_signal_switches_immediately() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        assert_eq!(m.activate(2).unwrap(), None);
        assert_eq!(m.active(), 2);
    }

    #[test]
    fn immediate_switch_starts_target_acquire_handshake() {
        let m = VtManager::new();
        m.mode(2, 9, [VT_PROCESS, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        assert_switch_signal(m.activate(2).unwrap(), (9, 2));
        assert_eq!(m.active(), 2);
        assert_eq!(m.release_reply(9, VT_ACKACQ), Ok(None));
    }
    #[test]
    fn disallocate_never_removes_active_console() {
        let m = VtManager::new();
        assert_eq!(m.disallocate(1), Err(AxError::ResourceBusy));
    }

    #[test]
    fn owner_exit_completes_a_pending_vt_process_switch() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        m.mode(2, 9, [VT_PROCESS, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        assert_switch_signal(m.activate(2).unwrap(), (7, 1));

        assert_switch_signal(m.owner_exited(7), (9, 2));
        assert_eq!(m.active(), 2);
        assert!(m.state.lock().vts[0].process.is_none());
    }

    #[test]
    fn owner_exit_keeps_acquire_pending_until_xorg_acknowledges() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        m.mode(2, 9, [VT_PROCESS, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        assert_switch_signal(m.activate(2).unwrap(), (7, 1));
        assert_switch_signal(m.owner_exited(7), (9, 2));
        assert_eq!(m.release_reply(9, VT_ACKACQ), Ok(None));
    }

    #[test]
    fn owner_exit_clears_pending_acquire_for_dead_owner() {
        let m = VtManager::new();
        m.mode(2, 9, [VT_PROCESS, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        assert_switch_signal(m.activate(2).unwrap(), (9, 2));
        assert_eq!(m.owner_exited(9), None);
        assert!(m.state.lock().pending.is_none());
        assert_eq!(m.release_reply(9, VT_ACKACQ), Err(AxError::InvalidInput));
    }

    #[test]
    fn replacing_process_mode_cancels_its_pending_release_handshake() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        assert_switch_signal(m.activate(2).unwrap(), (7, 1));

        // This is intentionally the same PID: a new VT_PROCESS mode still
        // invalidates the old mode instance's handshake.
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        assert!(m.state.lock().pending.is_none());
        assert_eq!(m.release_reply(7, 1), Err(AxError::InvalidInput));
        assert_eq!(m.active(), 1);
    }

    #[test]
    fn same_pid_mode_replacement_rejects_the_already_returned_signal_ticket() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        let ticket = m.activate(2).unwrap().unwrap();
        assert!(m.ticket_is_current(ticket));

        // Model VT_SETMODE winning the deterministic delivery-gate race
        // before the ioctl caller reaches deliver_switch_signal().
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        assert!(!m.ticket_is_current(ticket));
    }

    #[test]
    fn signal_claim_linearizes_before_same_pid_mode_replacement() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        let ticket = m.activate(2).unwrap().unwrap();
        assert!(m.claim_ticket_for_test(ticket));

        // In production VT_SETMODE observes this in-flight claim, waits for
        // completion without holding a VT lock, then becomes ordered after
        // the signal.  Finishing the claim models the signal return edge.
        m.finish_ticket_for_test(ticket);
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        assert!(!m.ticket_is_current(ticket));
    }

    #[test]
    fn clearing_process_mode_cancels_its_pending_acquire_handshake() {
        let m = VtManager::new();
        m.mode(2, 9, [VT_PROCESS, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        assert_switch_signal(m.activate(2).unwrap(), (9, 2));

        m.mode(2, 9, [VT_AUTO, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(m.state.lock().pending.is_none());
        assert_eq!(m.release_reply(9, VT_ACKACQ), Err(AxError::InvalidInput));
    }

    #[test]
    fn owner_exit_resets_graphics_and_chains_release_to_acquire() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        m.mode(2, 9, [VT_PROCESS, 0, 0, 0, 2, 0, 0, 0]).unwrap();
        m.state.lock().vts[0].graphics = true;
        m.state.lock().vts[0].kb_mode = K_OFF;
        assert_switch_signal(m.activate(2).unwrap(), (7, 1));
        assert_switch_signal(m.owner_exited(7), (9, 2));
        assert!(!m.graphics(1));
        assert_eq!(m.state.lock().vts[0].kb_mode, K_XLATE);
        assert_eq!(m.owner_exited(9), None);
        assert!(m.state.lock().pending.is_none());
    }

    #[test]
    fn input_batch_and_switch_share_the_route_linearization_gate() {
        let m = VtManager::new();
        let route = m.route.lock();
        let active = m.state.lock().active;
        assert_eq!(active, 1);
        drop(route);
        assert_eq!(m.activate(2), Ok(None));
        assert_eq!(m.active(), 2);
    }

    #[test]
    fn active_tty_alias_tracks_the_current_active_vt() {
        let m = VtManager::new();
        let tty1 = m.active_tty().0;
        assert_eq!(m.activate(2), Ok(None));
        assert!(!Arc::ptr_eq(&tty1, &m.active_tty().0));
    }

    #[test]
    fn alias_open_resolves_once_and_keeps_that_vt_busy() {
        let m = VtManager::new();
        // This is the state captured by VtDevice(0)::open_description.
        let resolved = m.active();
        let tty = m.tty_for(resolved);
        m.opened(resolved).unwrap();

        assert_eq!(m.activate(2), Ok(None));
        assert_eq!(resolved, 1);
        assert!(Arc::ptr_eq(&tty, &m.tty_for(1)));
        assert!(!Arc::ptr_eq(&tty, &m.active_tty().0));
        assert_eq!(m.disallocate(1), Err(AxError::ResourceBusy));
        m.closed(resolved);
    }

    #[test]
    fn erased_tty_identity_recovers_only_its_vt_number() {
        let m = VtManager::new();
        let tty: Arc<dyn Any + Send + Sync> = m.tty_for(2);
        assert_eq!(m.number_for_tty(tty.as_ref()), Some(2));
        let console: Arc<dyn Any + Send + Sync> = super::super::N_TTY.clone();
        assert_eq!(m.number_for_tty(console.as_ref()), None);
    }

    #[test]
    fn device_zero_is_the_active_console_alias() {
        assert!(VtDevice::new(0).is_active_alias());
        assert!(!VtDevice::new(1).is_active_alias());
    }

    #[test]
    fn xorg_release_acquire_handshake_requires_ackacq() {
        let m = VtManager::new();
        m.mode(1, 7, [VT_PROCESS, 0, 1, 0, 2, 0, 0, 0]).unwrap();
        m.mode(2, 9, [VT_PROCESS, 0, 1, 0, 2, 0, 0, 0]).unwrap();

        assert_switch_signal(m.activate(2).unwrap(), (7, 1));
        assert_switch_signal(m.release_reply(7, 1).unwrap(), (9, 2));
        assert_eq!(m.active(), 2);
        assert_eq!(m.release_reply(9, 1), Err(AxError::PermissionDenied));
        assert_eq!(m.release_reply(9, VT_ACKACQ), Ok(None));
        assert_eq!(m.release_reply(9, VT_ACKACQ), Err(AxError::InvalidInput));
    }

    #[test]
    fn open_lifetime_blocks_disallocate_but_not_final_close() {
        let m = VtManager::new();
        m.opened(2).unwrap();
        m.opened(2).unwrap(); // dup/fork references belong to the same OFD; this models two OFDs.
        assert_eq!(m.open_query(), 3);
        assert_eq!(m.disallocate(2), Err(AxError::ResourceBusy));
        m.closed(2);
        assert_eq!(m.disallocate(2), Err(AxError::ResourceBusy));
        m.closed(2);
        assert_eq!(m.disallocate(2), Ok(()));
    }

    #[test]
    fn getstate_mask_reserves_tty0_and_maps_vt1_to_bit_one() {
        let m = VtManager::new();
        m.opened(16).unwrap();
        m.opened(17).unwrap();
        let bytes = m.state_bytes();
        let mask = u16::from_ne_bytes(bytes[4..6].try_into().unwrap());
        assert_eq!(mask, 1 | (1 << 1));
        assert_eq!(m.open_query(), 2);
    }

    #[test]
    fn inactive_final_close_makes_vt_reusable_without_deallocating_storage() {
        let m = VtManager::new();
        m.opened(2).unwrap();
        m.closed(2);
        assert_eq!(m.open_query(), 2);
    }

    #[test]
    fn single_disallocate_restores_keyboard_translation_mode() {
        let m = VtManager::new();
        m.state.lock().vts[1].kb_mode = K_OFF;
        assert_eq!(m.disallocate(2), Ok(()));
        assert_eq!(m.state.lock().vts[1].kb_mode, K_XLATE);
    }

    #[test]
    fn text_presentation_excludes_inactive_and_graphics_vts() {
        let m = VtManager::new();
        assert_eq!(m.with_text_active(2, || 1), None);
        assert_eq!(m.with_text_active(1, || 1), Some(1));
        m.state.lock().vts[0].graphics = true;
        assert_eq!(m.with_text_active(1, || 1), None);
    }

    #[test]
    fn mutators_allow_controlling_tty_or_tty_config_capability() {
        assert!(can_control_vt(true, false));
        assert!(can_control_vt(false, true));
        assert!(!can_control_vt(false, false));
    }

    #[test]
    fn each_virtual_console_has_a_distinct_tty_session_identity() {
        let m = VtManager::new();
        assert!(!Arc::ptr_eq(&m.tty_for(1), &m.tty_for(2)));
    }

    #[test]
    fn ioctl_number_and_mode_arguments_do_not_truncate() {
        assert_eq!(vt_number_from_arg(65_537), Err(AxError::InvalidInput));
        assert_eq!(kd_mode_from_arg(usize::MAX), Err(AxError::InvalidInput));
    }

    #[test]
    fn process_mode_signal_numbers_do_not_truncate() {
        let m = VtManager::new();
        let signal_257 = 257i16.to_ne_bytes();
        assert_eq!(
            m.mode(
                1,
                7,
                [VT_PROCESS, 0, signal_257[0], signal_257[1], 0, 0, 0, 0,],
            ),
            Err(AxError::InvalidInput)
        );
    }
}
