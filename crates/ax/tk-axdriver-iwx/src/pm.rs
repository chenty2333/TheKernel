//! Suspend, wakeup and deferred-init recovery from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 (ISC).
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>; Copyright (c) 2014
//! Fixup Software Ltd.; Copyright (c) 2017, 2019, 2020 Stefan Sperling
//! <stsp@openbsd.org>.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitTaskAction {
    AcquireIoctlWrite,
    StopRunningInterface,
    ClearHardwareError,
    Initialize,
    ReleaseIoctlWrite,
    RestoreNetworkPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitTaskState {
    pub generation: u32,
    pub shutdown_generation: u32,
    pub running: bool,
    pub interface_up: bool,
    pub hardware_error: bool,
    pub rfkill: bool,
}

/// Recover from queued fatal/watchdog work only when generation and fatal gates permit it.
// upstream: if_iwx.c iwx_init_task()
pub fn run_init_task<E>(
    state: &mut InitTaskState,
    mut execute: impl FnMut(InitTaskAction) -> Result<(), E>,
) -> Result<(), E> {
    execute(InitTaskAction::AcquireIoctlWrite)?;
    if state.generation != state.shutdown_generation {
        execute(InitTaskAction::ReleaseIoctlWrite)?;
        execute(InitTaskAction::RestoreNetworkPriority)?;
        return Ok(());
    }
    if state.running {
        execute(InitTaskAction::StopRunningInterface)?;
        state.running = false;
    } else {
        state.hardware_error = false;
        execute(InitTaskAction::ClearHardwareError)?;
    }
    let fatal = state.hardware_error || state.rfkill;
    if !fatal && state.interface_up {
        execute(InitTaskAction::Initialize)?;
    }
    execute(InitTaskAction::ReleaseIoctlWrite)?;
    execute(InitTaskAction::RestoreNetworkPriority)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    ClearPciRetryTimeout,
    ClearPciInterruptDisable,
    DisableDeviceInterrupts,
}

/// Restore PCI retry and legacy interrupt command state before resuming.
// upstream: if_iwx.c iwx_resume()
pub fn resume_device(msix: bool, mut execute: impl FnMut(ResumeAction)) {
    execute(ResumeAction::ClearPciRetryTimeout);
    if !msix {
        execute(ResumeAction::ClearPciInterruptDisable);
    }
    execute(ResumeAction::DisableDeviceInterrupts);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeAction {
    AcquireIoctlWrite,
    StartHardware,
    InitHardware,
    StopDevice,
    InitializeTaskReferences,
    ClearOutputActive,
    MarkRunning,
    EnterRunState,
    BeginScan,
    ReleaseIoctlWrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeError<E> {
    Hardware(E),
}

/// Restart NIC, initialize firmware, then resume monitor or station operation.
// upstream: if_iwx.c iwx_wakeup()
pub fn wakeup_device<E>(
    monitor_mode: bool,
    mut execute: impl FnMut(WakeAction) -> Result<(), E>,
) -> Result<(), WakeError<E>> {
    execute(WakeAction::AcquireIoctlWrite).map_err(WakeError::Hardware)?;
    if let Err(error) = execute(WakeAction::StartHardware) {
        let _ = execute(WakeAction::ReleaseIoctlWrite);
        return Err(WakeError::Hardware(error));
    }
    if let Err(error) = execute(WakeAction::InitHardware) {
        let _ = execute(WakeAction::StopDevice);
        let _ = execute(WakeAction::ReleaseIoctlWrite);
        return Err(WakeError::Hardware(error));
    }
    for action in [
        WakeAction::InitializeTaskReferences,
        WakeAction::ClearOutputActive,
        WakeAction::MarkRunning,
    ] {
        execute(action).map_err(WakeError::Hardware)?;
    }
    execute(if monitor_mode {
        WakeAction::EnterRunState
    } else {
        WakeAction::BeginScan
    })
    .map_err(WakeError::Hardware)?;
    execute(WakeAction::ReleaseIoctlWrite).map_err(WakeError::Hardware)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Quiesce,
    Resume,
    Wakeup,
    Other,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationAction {
    Stop,
    Resume,
    Wakeup,
}

/// Translate the device activation hooks for quiesce/resume/wakeup.
// upstream: if_iwx.c iwx_activate()
pub fn activation_actions(
    activation: Activation,
    interface_running: bool,
    interface_up: bool,
    _msix: bool,
) -> Option<ActivationAction> {
    match activation {
        Activation::Quiesce if interface_running => Some(ActivationAction::Stop),
        Activation::Resume => Some(ActivationAction::Resume),
        Activation::Wakeup if interface_up && !interface_running => Some(ActivationAction::Wakeup),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn init_recovery_honors_generation_and_fatal_gates() {
        let mut actions = Vec::new();
        let mut state = InitTaskState {
            generation: 3,
            shutdown_generation: 3,
            running: false,
            interface_up: true,
            hardware_error: true,
            rfkill: false,
        };
        assert_eq!(
            run_init_task(&mut state, |step| {
                actions.push(step);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(
            actions,
            [
                InitTaskAction::AcquireIoctlWrite,
                InitTaskAction::ClearHardwareError,
                InitTaskAction::Initialize,
                InitTaskAction::ReleaseIoctlWrite,
                InitTaskAction::RestoreNetworkPriority
            ]
        );
        actions.clear();
        state.running = true;
        state.rfkill = true;
        assert_eq!(
            run_init_task(&mut state, |step| {
                actions.push(step);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(
            actions,
            [
                InitTaskAction::AcquireIoctlWrite,
                InitTaskAction::StopRunningInterface,
                InitTaskAction::ReleaseIoctlWrite,
                InitTaskAction::RestoreNetworkPriority
            ]
        );
    }

    #[test]
    fn resume_and_wakeup_keep_source_paths() {
        let mut resume = Vec::new();
        resume_device(false, |step| resume.push(step));
        assert_eq!(
            resume,
            [
                ResumeAction::ClearPciRetryTimeout,
                ResumeAction::ClearPciInterruptDisable,
                ResumeAction::DisableDeviceInterrupts
            ]
        );
        let mut wake = Vec::new();
        assert_eq!(
            wakeup_device(false, |step| {
                wake.push(step);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(
            wake,
            [
                WakeAction::AcquireIoctlWrite,
                WakeAction::StartHardware,
                WakeAction::InitHardware,
                WakeAction::InitializeTaskReferences,
                WakeAction::ClearOutputActive,
                WakeAction::MarkRunning,
                WakeAction::BeginScan,
                WakeAction::ReleaseIoctlWrite
            ]
        );
        assert_eq!(
            activation_actions(Activation::Wakeup, false, true, false),
            Some(ActivationAction::Wakeup)
        );
        assert_eq!(
            activation_actions(Activation::Wakeup, true, true, false),
            None
        );
    }
}
