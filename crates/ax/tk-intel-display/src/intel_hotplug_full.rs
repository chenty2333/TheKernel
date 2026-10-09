// Copyright © 2015 Intel Corporation
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice (including the next
// paragraph) shall be included in all copies or substantial portions of the
// Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
// THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
// IN THE SOFTWARE.
// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_hotplug.c.
//! HPD storm handling, IRQ bottom halves, retries, polling and pin blocking.
//!
//! Linux/DRM framework calls, locks, power references and workqueues are explicit
//! `HotplugIo` operations. The owner serializes mutable display state exactly at
//! the source lock boundaries. Connector and encoder indices are stable backend
//! handles, not DRM object pointers.
//!
//! The following source functions are intentionally omitted as debugfs-only
//! framework adapters: `i915_hpd_storm_ctl_show`,
//! `i915_hpd_storm_ctl_write`, `i915_hpd_storm_ctl_open`,
//! `i915_hpd_short_storm_ctl_show`, `i915_hpd_short_storm_ctl_open`,
//! `i915_hpd_short_storm_ctl_write`, and `intel_hpd_debugfs_register`.

extern crate alloc;
use alloc::vec::Vec;

pub type HpdPin = u8;
pub const HPD_NONE: HpdPin = 0;
pub const HPD_NUM_PINS: usize = 16;
pub const HPD_PORT_A: HpdPin = 4;
pub const PORT_A: u8 = 0;
pub const HPD_STORM_DEFAULT_THRESHOLD: i32 = 50;
pub const HPD_STORM_DETECT_PERIOD_MS: u64 = 1_000;
pub const HPD_STORM_REENABLE_DELAY_MS: u64 = 2 * 60 * 1_000;
pub const HPD_RETRY_DELAY_MS: u64 = 1_000;

pub const DRM_CONNECTOR_POLL_HPD: u32 = 1 << 0;
pub const DRM_CONNECTOR_POLL_CONNECT: u32 = 1 << 1;
pub const DRM_CONNECTOR_POLL_DISCONNECT: u32 = 1 << 2;

const fn pin_bit(pin: HpdPin) -> u32 {
    if pin < 32 { 1u32 << pin } else { 0 }
}

const fn for_each_pin() -> core::ops::Range<HpdPin> {
    (HPD_NONE + 1)..(HPD_NUM_PINS as HpdPin)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HpdState {
    Enabled,
    Disabled,
    MarkDisabled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorStatus {
    Unknown,
    Connected,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelHotplugState {
    Unchanged,
    Changed,
    Retry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrqReturn {
    None,
    Handled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Work {
    Hotplug,
    DigPort,
    PollInit,
    Reenable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkQueue {
    Unordered,
    DisplayPort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HotplugLog {
    StormDetected {
        pin: HpdPin,
    },
    IrqCount {
        pin: HpdPin,
        count: i32,
    },
    SwitchToPolling {
        connector: usize,
    },
    ReenableHpd {
        connector: usize,
    },
    ConnectorStatusChanged {
        connector: usize,
        old: ConnectorStatus,
        new: ConnectorStatus,
        old_epoch: u64,
        new_epoch: u64,
    },
    RunningEncoderHotplug,
    ConnectorEvent {
        connector: usize,
        pin: HpdPin,
        retry: u32,
    },
    IgnoreLongHpd,
    UnexpectedDisabledIrq {
        pin: HpdPin,
    },
    DigitalHpd {
        encoder: usize,
        long: bool,
    },
    DetectionWorkStillActive,
    ModeConfigLockMissing,
    BlockedPinUnderflow {
        pin: HpdPin,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HpdStats {
    pub last_jiffies: u64,
    pub count: i32,
    pub state: HpdState,
    pub blocked_count: u32,
}

impl Default for HpdStats {
    fn default() -> Self {
        Self {
            last_jiffies: 0,
            count: 0,
            state: HpdState::Enabled,
            blocked_count: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HotplugState {
    pub stats: [HpdStats; HPD_NUM_PINS],
    pub hpd_storm_threshold: i32,
    pub hpd_short_storm_enabled: bool,
    pub detection_work_enabled: bool,
    pub poll_enabled: bool,
    pub ignore_long_hpd: bool,
    pub event_bits: u32,
    pub retry_bits: u32,
    pub long_hpd_pin_mask: u32,
    pub short_hpd_pin_mask: u32,
    /// Mirrors the source callsite's `drm_WARN_ONCE` for disabled-pin IRQs.
    pub warned_unexpected_disabled_irq: bool,
}

impl Default for HotplugState {
    fn default() -> Self {
        Self {
            stats: [HpdStats::default(); HPD_NUM_PINS],
            hpd_storm_threshold: HPD_STORM_DEFAULT_THRESHOLD,
            hpd_short_storm_enabled: false,
            detection_work_enabled: false,
            poll_enabled: false,
            ignore_long_hpd: false,
            event_bits: 0,
            retry_bits: 0,
            long_hpd_pin_mask: 0,
            short_hpd_pin_mask: 0,
            warned_unexpected_disabled_irq: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Encoder {
    pub name: &'static str,
    pub hpd_pin: HpdPin,
    pub digital_port: bool,
    pub dp_encoder: bool,
    pub has_hpd_pulse: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Connector {
    pub name: &'static str,
    pub hpd_pin_encoder: Option<usize>,
    /// DRM connector's polling flags as currently installed.
    pub polled: u32,
    /// i915's intended polling flags, restored after temporary storm polling.
    pub desired_polled: u32,
    pub status: ConnectorStatus,
    pub epoch_counter: u64,
    pub force: bool,
    pub hotplug_retries: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectorProbe {
    pub status: ConnectorStatus,
    pub epoch_counter: u64,
}

/// Host operations corresponding to i915, DRM, power-management and workqueue APIs.
/// No operation is silently emulated; a backend supplies lock and scheduling effects.
pub trait HotplugIo {
    type RuntimePowerRef;
    type DisplayCorePowerRef;

    /// `spin_lock()`; used by the top-level IRQ handler.
    fn irq_lock(&mut self);
    fn irq_unlock(&mut self);
    /// `spin_lock_irq()` used by the worker/task-context callsites.
    fn irq_lock_irq(&mut self);
    fn irq_unlock_irq(&mut self);
    fn irq_lock_irqsave(&mut self);
    fn irq_unlock_irqrestore(&mut self);
    fn mode_config_lock(&mut self);
    fn mode_config_unlock(&mut self);
    fn mode_config_is_locked(&self) -> bool;

    fn jiffies(&self) -> u64;
    fn msecs_to_jiffies(&self, milliseconds: u64) -> u64;
    fn init_work(&mut self, work: Work, delayed: bool);
    fn queue_work(&mut self, queue: WorkQueue, work: Work) -> bool;
    /// `modify=true` is `mod_delayed_work`; otherwise this is `queue_delayed_work`.
    fn queue_delayed_work(
        &mut self,
        queue: WorkQueue,
        work: Work,
        delay_jiffies: u64,
        modify: bool,
    ) -> bool;
    fn cancel_work(&mut self, work: Work) -> bool;
    fn cancel_work_sync(&mut self, work: Work) -> bool;
    fn cancel_delayed_work_sync(&mut self, work: Work) -> bool;
    fn flush_work(&mut self, work: Work);
    fn flush_delayed_work(&mut self, work: Work);
    fn delayed_work_pending(&self, work: Work) -> bool;

    fn hpd_irq_setup(&mut self);
    fn hpd_pulse(&mut self, encoder: usize, long: bool) -> IrqReturn;
    fn connector_probe_detect(&mut self, connector: usize, old: ConnectorProbe) -> ConnectorProbe;
    fn encoder_hotplug(&mut self, encoder: usize, connector: usize) -> IntelHotplugState;
    fn connector_get(&mut self, connector: usize);
    fn connector_put(&mut self, connector: usize);
    fn connector_hotplug_event(&mut self, connector: Option<usize>);
    fn poll_reschedule(&mut self);

    fn runtime_power_get(&mut self) -> Self::RuntimePowerRef;
    fn runtime_power_put(&mut self, reference: Self::RuntimePowerRef);
    fn display_core_power_get(&mut self) -> Self::DisplayCorePowerRef;
    fn display_core_power_put(&mut self, reference: Self::DisplayCorePowerRef);
    fn dp_dpcd_set_probe(&mut self, encoder: usize, enabled: bool);
    fn cancel_connector_modeset_retry(&mut self, connector: usize);
    fn cancel_connector_hdcp_works(&mut self, connector: usize);
    fn trace(&mut self, event: HotplugLog);
}

/// Mutable i915 display state consumed by this translation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntelHotplugDisplay {
    pub has_display: bool,
    pub device_enabled: bool,
    pub has_dp_mst: bool,
    pub has_gmch: bool,
    pub mode_config_poll_enabled: bool,
    pub hotplug: HotplugState,
    pub encoders: Vec<Encoder>,
    pub connectors: Vec<Connector>,
}

impl IntelHotplugDisplay {
    // upstream: intel_hotplug.c intel_hpd_pin_default()
    /// Return the default HPD pin associated with a digital port.
    pub const fn intel_hpd_pin_default(port: u8) -> HpdPin {
        HPD_PORT_A + port - PORT_A
    }

    // upstream: intel_hotplug.c intel_connector_hpd_pin()
    fn intel_connector_hpd_pin(&self, connector: usize) -> HpdPin {
        self.connectors
            .get(connector)
            .and_then(|c| c.hpd_pin_encoder)
            .and_then(|e| self.encoders.get(e))
            .map_or(HPD_NONE, |encoder| encoder.hpd_pin)
    }

    // upstream: intel_hotplug.c intel_hpd_irq_storm_detect()
    fn intel_hpd_irq_storm_detect<I: HotplugIo>(
        &mut self,
        io: &mut I,
        pin: HpdPin,
        long_hpd: bool,
    ) -> bool {
        let hpd = &mut self.hotplug;
        let start = hpd.stats[pin as usize].last_jiffies;
        let end = start.wrapping_add(io.msecs_to_jiffies(HPD_STORM_DETECT_PERIOD_MS));
        let increment = if long_hpd { 10 } else { 1 };
        let threshold = hpd.hpd_storm_threshold;
        let mut storm = false;

        if threshold == 0 || (!long_hpd && !hpd.hpd_short_storm_enabled) {
            return false;
        }

        // Linux time_in_range() uses wrap-safe unsigned comparisons.
        let now = io.jiffies();
        if now.wrapping_sub(start) > end.wrapping_sub(start) {
            hpd.stats[pin as usize].last_jiffies = io.jiffies();
            hpd.stats[pin as usize].count = 0;
        }

        hpd.stats[pin as usize].count += increment;
        if hpd.stats[pin as usize].count > threshold {
            hpd.stats[pin as usize].state = HpdState::MarkDisabled;
            io.trace(HotplugLog::StormDetected { pin });
            storm = true;
        } else {
            io.trace(HotplugLog::IrqCount {
                pin,
                count: hpd.stats[pin as usize].count,
            });
        }
        storm
    }

    // upstream: intel_hotplug.c detection_work_enabled()
    fn detection_work_enabled(&self) -> bool {
        self.hotplug.detection_work_enabled
    }

    // upstream: intel_hotplug.c mod_delayed_detection_work()
    fn mod_delayed_detection_work<I: HotplugIo>(
        &self,
        io: &mut I,
        work: Work,
        delay_jiffies: u64,
    ) -> bool {
        if !self.detection_work_enabled() {
            return false;
        }
        io.queue_delayed_work(WorkQueue::Unordered, work, delay_jiffies, true)
    }

    // upstream: intel_hotplug.c queue_delayed_detection_work()
    fn queue_delayed_detection_work<I: HotplugIo>(
        &self,
        io: &mut I,
        work: Work,
        delay_jiffies: u64,
    ) -> bool {
        if !self.detection_work_enabled() {
            return false;
        }
        io.queue_delayed_work(WorkQueue::Unordered, work, delay_jiffies, false)
    }

    // upstream: intel_hotplug.c queue_detection_work()
    fn queue_detection_work<I: HotplugIo>(&self, io: &mut I, work: Work) -> bool {
        if !self.detection_work_enabled() {
            return false;
        }
        io.queue_work(WorkQueue::Unordered, work)
    }

    // upstream: intel_hotplug.c intel_hpd_irq_storm_switch_to_polling()
    fn intel_hpd_irq_storm_switch_to_polling<I: HotplugIo>(&mut self, io: &mut I) {
        let mut hpd_disabled = false;
        for connector in 0..self.connectors.len() {
            if self.connectors[connector].polled != DRM_CONNECTOR_POLL_HPD {
                continue;
            }
            let pin = self.intel_connector_hpd_pin(connector);
            if pin == HPD_NONE || self.hotplug.stats[pin as usize].state != HpdState::MarkDisabled {
                continue;
            }
            io.trace(HotplugLog::SwitchToPolling { connector });
            self.hotplug.stats[pin as usize].state = HpdState::Disabled;
            self.connectors[connector].polled =
                DRM_CONNECTOR_POLL_CONNECT | DRM_CONNECTOR_POLL_DISCONNECT;
            hpd_disabled = true;
        }

        // Enable polling and queue hotplug re-enabling.
        if hpd_disabled {
            io.poll_reschedule();
            self.mod_delayed_detection_work(
                io,
                Work::Reenable,
                io.msecs_to_jiffies(HPD_STORM_REENABLE_DELAY_MS),
            );
        }
    }

    // upstream: intel_hotplug.c intel_hpd_irq_storm_reenable_work()
    pub fn intel_hpd_irq_storm_reenable_work<I: HotplugIo>(&mut self, io: &mut I) {
        let wakeref = io.runtime_power_get();
        io.irq_lock_irq();

        for connector in 0..self.connectors.len() {
            let pin = self.intel_connector_hpd_pin(connector);
            if pin == HPD_NONE || self.hotplug.stats[pin as usize].state != HpdState::Disabled {
                continue;
            }
            if self.connectors[connector].polled != self.connectors[connector].desired_polled {
                io.trace(HotplugLog::ReenableHpd { connector });
            }
            self.connectors[connector].polled = self.connectors[connector].desired_polled;
        }

        for pin in for_each_pin() {
            if self.hotplug.stats[pin as usize].state == HpdState::Disabled {
                self.hotplug.stats[pin as usize].state = HpdState::Enabled;
            }
        }
        io.hpd_irq_setup();
        io.irq_unlock_irq();
        io.runtime_power_put(wakeref);
    }

    // upstream: intel_hotplug.c intel_hotplug_detect_connector()
    fn intel_hotplug_detect_connector<I: HotplugIo>(
        &mut self,
        io: &mut I,
        connector: usize,
    ) -> IntelHotplugState {
        if !io.mode_config_is_locked() {
            io.trace(HotplugLog::ModeConfigLockMissing);
        }
        let old_status = self.connectors[connector].status;
        let old_epoch_counter = self.connectors[connector].epoch_counter;
        let old = ConnectorProbe {
            status: old_status,
            epoch_counter: old_epoch_counter,
        };
        let probe = io.connector_probe_detect(connector, old);
        if !self.connectors[connector].force {
            self.connectors[connector].status = probe.status;
        }
        self.connectors[connector].epoch_counter = probe.epoch_counter;

        if old_epoch_counter != self.connectors[connector].epoch_counter {
            io.trace(HotplugLog::ConnectorStatusChanged {
                connector,
                old: old_status,
                new: self.connectors[connector].status,
                old_epoch: old_epoch_counter,
                new_epoch: self.connectors[connector].epoch_counter,
            });
            IntelHotplugState::Changed
        } else {
            IntelHotplugState::Unchanged
        }
    }

    // upstream: intel_hotplug.c intel_encoder_hotplug()
    pub fn intel_encoder_hotplug<I: HotplugIo>(
        &mut self,
        io: &mut I,
        _encoder: usize,
        connector: usize,
    ) -> IntelHotplugState {
        self.intel_hotplug_detect_connector(io, connector)
    }

    // upstream: intel_hotplug.c intel_encoder_has_hpd_pulse()
    fn intel_encoder_has_hpd_pulse(&self, encoder: usize) -> bool {
        self.encoders
            .get(encoder)
            .is_some_and(|e| e.digital_port && e.has_hpd_pulse)
    }

    // upstream: intel_hotplug.c hpd_pin_has_pulse()
    fn hpd_pin_has_pulse(&self, pin: HpdPin) -> bool {
        self.encoders.iter().enumerate().any(|(index, encoder)| {
            encoder.hpd_pin == pin && self.intel_encoder_has_hpd_pulse(index)
        })
    }

    // upstream: intel_hotplug.c hpd_pin_is_blocked()
    fn hpd_pin_is_blocked(&self, pin: HpdPin) -> bool {
        self.hotplug.stats[pin as usize].blocked_count != 0
    }

    // upstream: intel_hotplug.c get_blocked_hpd_pin_mask()
    fn get_blocked_hpd_pin_mask(&self) -> u32 {
        let mut mask = 0;
        for pin in for_each_pin() {
            if self.hpd_pin_is_blocked(pin) {
                mask |= pin_bit(pin);
            }
        }
        mask
    }

    // upstream: intel_hotplug.c i915_digport_work_func()
    pub fn i915_digport_work_func<I: HotplugIo>(&mut self, io: &mut I) {
        io.irq_lock_irq();
        let blocked_hpd_pin_mask = self.get_blocked_hpd_pin_mask();
        let long_hpd_pin_mask = self.hotplug.long_hpd_pin_mask & !blocked_hpd_pin_mask;
        self.hotplug.long_hpd_pin_mask &= !long_hpd_pin_mask;
        let short_hpd_pin_mask = self.hotplug.short_hpd_pin_mask & !blocked_hpd_pin_mask;
        self.hotplug.short_hpd_pin_mask &= !short_hpd_pin_mask;
        io.irq_unlock_irq();

        let mut old_bits = 0;
        for encoder in 0..self.encoders.len() {
            if !self.intel_encoder_has_hpd_pulse(encoder) {
                continue;
            }
            let pin = self.encoders[encoder].hpd_pin;
            let long_hpd = long_hpd_pin_mask & pin_bit(pin) != 0;
            let short_hpd = short_hpd_pin_mask & pin_bit(pin) != 0;
            if !long_hpd && !short_hpd {
                continue;
            }
            if io.hpd_pulse(encoder, long_hpd) == IrqReturn::None {
                // fall back to old school hpd
                old_bits |= pin_bit(pin);
            }
        }

        if old_bits != 0 {
            io.irq_lock_irq();
            self.hotplug.event_bits |= old_bits;
            self.queue_delayed_detection_work(io, Work::Hotplug, 0);
            io.irq_unlock_irq();
        }
    }

    // upstream: intel_hotplug.c intel_hpd_trigger_irq()
    /// Emulate a short sink pulse and schedule DisplayPort pulse processing.
    pub fn intel_hpd_trigger_irq<I: HotplugIo>(&mut self, io: &mut I, encoder: usize) {
        let pin = self.encoders[encoder].hpd_pin;
        io.irq_lock_irq();
        self.hotplug.short_hpd_pin_mask |= pin_bit(pin);
        if !self.hpd_pin_is_blocked(pin) {
            io.queue_work(WorkQueue::DisplayPort, Work::DigPort);
        }
        io.irq_unlock_irq();
    }

    // upstream: intel_hotplug.c i915_hotplug_work_func()
    pub fn i915_hotplug_work_func<I: HotplugIo>(&mut self, io: &mut I) {
        io.mode_config_lock();
        io.trace(HotplugLog::RunningEncoderHotplug);

        io.irq_lock_irq();
        let blocked_hpd_pin_mask = self.get_blocked_hpd_pin_mask();
        let hpd_event_bits = self.hotplug.event_bits & !blocked_hpd_pin_mask;
        self.hotplug.event_bits &= !hpd_event_bits;
        let hpd_retry_bits = self.hotplug.retry_bits & !blocked_hpd_pin_mask;
        self.hotplug.retry_bits &= !hpd_retry_bits;

        // Enable polling for connectors which had HPD IRQ storms.
        self.intel_hpd_irq_storm_switch_to_polling(io);
        io.irq_unlock_irq();

        // Skip encoder hotplug handlers if ignore long HPD is set.
        if self.hotplug.ignore_long_hpd {
            io.trace(HotplugLog::IgnoreLongHpd);
            io.mode_config_unlock();
            return;
        }

        let mut changed = 0;
        let mut retry = 0;
        let mut first_changed_connector = None;
        let mut changed_connectors = 0;
        for connector in 0..self.connectors.len() {
            let pin = self.intel_connector_hpd_pin(connector);
            if pin == HPD_NONE {
                continue;
            }
            let hpd_bit = pin_bit(pin);
            if (hpd_event_bits | hpd_retry_bits) & hpd_bit == 0 {
                continue;
            }

            if hpd_event_bits & hpd_bit != 0 {
                self.connectors[connector].hotplug_retries = 0;
            } else {
                self.connectors[connector].hotplug_retries += 1;
            }
            io.trace(HotplugLog::ConnectorEvent {
                connector,
                pin,
                retry: self.connectors[connector].hotplug_retries,
            });

            let result = if let Some(encoder) = self.connectors[connector].hpd_pin_encoder {
                io.encoder_hotplug(encoder, connector)
            } else {
                // A connector with no attached encoder has HPD_NONE in i915.
                IntelHotplugState::Unchanged
            };
            match result {
                IntelHotplugState::Unchanged => {}
                IntelHotplugState::Changed => {
                    changed |= hpd_bit;
                    changed_connectors += 1;
                    if first_changed_connector.is_none() {
                        io.connector_get(connector);
                        first_changed_connector = Some(connector);
                    }
                }
                IntelHotplugState::Retry => retry |= hpd_bit,
            }
        }
        io.mode_config_unlock();

        if changed_connectors == 1 {
            io.connector_hotplug_event(first_changed_connector);
        } else if changed_connectors > 0 {
            io.connector_hotplug_event(None);
        }
        if let Some(connector) = first_changed_connector {
            io.connector_put(connector);
        }

        // Remove shared HPD pins that have changed.
        retry &= !changed;
        if retry != 0 {
            io.irq_lock_irq();
            self.hotplug.retry_bits |= retry;
            self.mod_delayed_detection_work(
                io,
                Work::Hotplug,
                io.msecs_to_jiffies(HPD_RETRY_DELAY_MS),
            );
            io.irq_unlock_irq();
        }
    }

    // upstream: intel_hotplug.c intel_hpd_irq_handler()
    pub fn intel_hpd_irq_handler<I: HotplugIo>(
        &mut self,
        io: &mut I,
        pin_mask: u32,
        long_mask: u32,
    ) {
        if pin_mask == 0 {
            return;
        }

        io.irq_lock();
        let mut storm_detected = false;
        let mut queue_dig = false;
        let mut queue_hp = false;
        let mut long_hpd_pulse_mask = 0;
        let mut short_hpd_pulse_mask = 0;

        // Determine whether each pin has a pulse handler and whether it was long.
        for encoder in 0..self.encoders.len() {
            let pin = self.encoders[encoder].hpd_pin;
            let bit = pin_bit(pin);
            if pin_mask & bit == 0 || !self.intel_encoder_has_hpd_pulse(encoder) {
                continue;
            }
            let long_hpd = long_mask & bit != 0;
            io.trace(HotplugLog::DigitalHpd {
                encoder,
                long: long_hpd,
            });
            if !self.hpd_pin_is_blocked(pin) {
                queue_dig = true;
            }
            if long_hpd {
                long_hpd_pulse_mask |= bit;
                self.hotplug.long_hpd_pin_mask |= bit;
            } else {
                short_hpd_pulse_mask |= bit;
                self.hotplug.short_hpd_pin_mask |= bit;
            }
        }

        // Now process each pin just once.
        for pin in for_each_pin() {
            let bit = pin_bit(pin);
            if pin_mask & bit == 0 {
                continue;
            }
            if self.hotplug.stats[pin as usize].state == HpdState::Disabled {
                // GMCH masks IRQ generation but may still set the HPD bits.
                if !self.has_gmch && !self.hotplug.warned_unexpected_disabled_irq {
                    self.hotplug.warned_unexpected_disabled_irq = true;
                    io.trace(HotplugLog::UnexpectedDisabledIrq { pin });
                }
                continue;
            }
            if self.hotplug.stats[pin as usize].state != HpdState::Enabled {
                continue;
            }

            // Delegate to hpd_pulse() when present; otherwise regular hotplug work.
            let long_hpd = if (short_hpd_pulse_mask | long_hpd_pulse_mask) & bit != 0 {
                long_hpd_pulse_mask & bit != 0
            } else {
                self.hotplug.event_bits |= bit;
                if !self.hpd_pin_is_blocked(pin) {
                    queue_hp = true;
                }
                true
            };

            if self.intel_hpd_irq_storm_detect(io, pin, long_hpd) {
                self.hotplug.event_bits &= !bit;
                storm_detected = true;
                queue_hp = true;
            }
        }

        // Disable storming IRQs now; polling is enabled later in hotplug work.
        if storm_detected {
            io.hpd_irq_setup();
        }

        // Hotplug callbacks can take modeset locks, so use dedicated workqueues.
        if queue_dig {
            io.queue_work(WorkQueue::DisplayPort, Work::DigPort);
        }
        if queue_hp {
            self.queue_delayed_detection_work(io, Work::Hotplug, 0);
        }
        io.irq_unlock();
    }

    // upstream: intel_hotplug.c intel_hpd_init()
    pub fn intel_hpd_init<I: HotplugIo>(&mut self, io: &mut I) {
        if !self.has_display {
            return;
        }
        for pin in for_each_pin() {
            self.hotplug.stats[pin as usize].count = 0;
            self.hotplug.stats[pin as usize].state = HpdState::Enabled;
        }
        // IRQ setup is single-threaded here; lock for the source assertion contract.
        io.irq_lock_irq();
        io.hpd_irq_setup();
        io.irq_unlock_irq();
    }

    // upstream: intel_hotplug.c i915_hpd_poll_detect_connectors()
    fn i915_hpd_poll_detect_connectors<I: HotplugIo>(&mut self, io: &mut I) {
        io.mode_config_lock();
        if !self.mode_config_poll_enabled {
            io.mode_config_unlock();
            return;
        }

        let mut first_changed_connector = None;
        let mut changed = 0;
        for connector in 0..self.connectors.len() {
            if self.connectors[connector].polled & DRM_CONNECTOR_POLL_HPD == 0 {
                continue;
            }
            if self.intel_hotplug_detect_connector(io, connector) != IntelHotplugState::Changed {
                continue;
            }
            changed += 1;
            if changed == 1 {
                io.connector_get(connector);
                first_changed_connector = Some(connector);
            }
        }
        io.mode_config_unlock();

        if changed == 0 {
            return;
        }
        if changed == 1 {
            io.connector_hotplug_event(first_changed_connector);
        } else {
            io.connector_hotplug_event(None);
        }
        if let Some(connector) = first_changed_connector {
            io.connector_put(connector);
        }
    }

    // upstream: intel_hotplug.c i915_hpd_poll_init_work()
    pub fn i915_hpd_poll_init_work<I: HotplugIo>(&mut self, io: &mut I) {
        io.mode_config_lock();
        let enabled = self.hotplug.poll_enabled;
        let wakeref = if !enabled {
            let wakeref = io.display_core_power_get();
            if self.hotplug.poll_enabled {
                io.trace(HotplugLog::ModeConfigLockMissing);
            }
            io.cancel_work(Work::PollInit);
            Some(wakeref)
        } else {
            None
        };

        io.irq_lock_irq();
        for connector in 0..self.connectors.len() {
            let pin = self.intel_connector_hpd_pin(connector);
            if pin == HPD_NONE || self.hotplug.stats[pin as usize].state == HpdState::Disabled {
                continue;
            }
            self.connectors[connector].polled = self.connectors[connector].desired_polled;
            if enabled && self.connectors[connector].polled == DRM_CONNECTOR_POLL_HPD {
                self.connectors[connector].polled =
                    DRM_CONNECTOR_POLL_CONNECT | DRM_CONNECTOR_POLL_DISCONNECT;
            }
        }
        io.irq_unlock_irq();

        if enabled {
            io.poll_reschedule();
        }
        io.mode_config_unlock();

        // Detect hotplugs possibly missed while polling was being disabled.
        if let Some(wakeref) = wakeref {
            self.i915_hpd_poll_detect_connectors(io);
            io.display_core_power_put(wakeref);
        }
    }

    // upstream: intel_hotplug.c intel_hpd_poll_enable()
    pub fn intel_hpd_poll_enable<I: HotplugIo>(&mut self, io: &mut I) {
        if !self.has_display || !self.device_enabled {
            return;
        }
        self.hotplug.poll_enabled = true;
        // Run separately because mode_config.mutex may already be held by caller.
        io.irq_lock_irq();
        self.queue_detection_work(io, Work::PollInit);
        io.irq_unlock_irq();
    }

    // upstream: intel_hotplug.c intel_hpd_poll_disable()
    pub fn intel_hpd_poll_disable<I: HotplugIo>(&mut self, io: &mut I) {
        if !self.has_display {
            return;
        }
        for encoder in 0..self.encoders.len() {
            if self.encoders[encoder].dp_encoder {
                io.dp_dpcd_set_probe(encoder, true);
            }
        }
        self.hotplug.poll_enabled = false;
        io.irq_lock_irq();
        self.queue_detection_work(io, Work::PollInit);
        io.irq_unlock_irq();
    }

    // upstream: intel_hotplug.c intel_hpd_poll_fini()
    pub fn intel_hpd_poll_fini<I: HotplugIo>(&mut self, io: &mut I) {
        for connector in 0..self.connectors.len() {
            io.cancel_connector_modeset_retry(connector);
            io.cancel_connector_hdcp_works(connector);
        }
    }

    // upstream: intel_hotplug.c intel_hpd_init_early()
    pub fn intel_hpd_init_early<I: HotplugIo>(&mut self, io: &mut I) {
        io.init_work(Work::Hotplug, true);
        io.init_work(Work::DigPort, false);
        io.init_work(Work::PollInit, false);
        io.init_work(Work::Reenable, true);
        self.hotplug.hpd_storm_threshold = HPD_STORM_DEFAULT_THRESHOLD;
        // Avoid false short-pulse storms caused by DP MST sideband messaging.
        self.hotplug.hpd_short_storm_enabled = !self.has_dp_mst;
    }

    // upstream: intel_hotplug.c cancel_all_detection_work()
    fn cancel_all_detection_work<I: HotplugIo>(&mut self, io: &mut I) -> bool {
        let mut was_pending = false;
        if io.cancel_delayed_work_sync(Work::Hotplug) {
            was_pending = true;
        }
        if io.cancel_work_sync(Work::PollInit) {
            was_pending = true;
        }
        if io.cancel_delayed_work_sync(Work::Reenable) {
            was_pending = true;
        }
        was_pending
    }

    // upstream: intel_hotplug.c intel_hpd_cancel_work()
    pub fn intel_hpd_cancel_work<I: HotplugIo>(&mut self, io: &mut I) {
        if !self.has_display {
            return;
        }
        io.irq_lock_irq();
        self.hotplug.long_hpd_pin_mask = 0;
        self.hotplug.short_hpd_pin_mask = 0;
        self.hotplug.event_bits = 0;
        self.hotplug.retry_bits = 0;
        io.irq_unlock_irq();

        io.cancel_work_sync(Work::DigPort);
        // All other work triggered by HPD should have been canceled by now.
        if self.cancel_all_detection_work(io) {
            io.trace(HotplugLog::DetectionWorkStillActive);
        }
    }

    // upstream: intel_hotplug.c queue_work_for_missed_irqs()
    fn queue_work_for_missed_irqs<I: HotplugIo>(&mut self, io: &mut I) {
        let hotplug = &self.hotplug;
        let blocked_hpd_pin_mask = self.get_blocked_hpd_pin_mask();
        let mut queue_hp_work =
            (hotplug.event_bits | hotplug.retry_bits) & !blocked_hpd_pin_mask != 0;

        for pin in for_each_pin() {
            match self.hotplug.stats[pin as usize].state {
                HpdState::MarkDisabled => queue_hp_work = true,
                HpdState::Disabled | HpdState::Enabled => {}
            }
        }
        if (self.hotplug.long_hpd_pin_mask | self.hotplug.short_hpd_pin_mask)
            & !blocked_hpd_pin_mask
            != 0
        {
            io.queue_work(WorkQueue::DisplayPort, Work::DigPort);
        }
        if queue_hp_work {
            self.queue_delayed_detection_work(io, Work::Hotplug, 0);
        }
    }

    // upstream: intel_hotplug.c block_hpd_pin()
    fn block_hpd_pin(&mut self, pin: HpdPin) -> bool {
        let blocked_count = &mut self.hotplug.stats[pin as usize].blocked_count;
        *blocked_count = blocked_count.wrapping_add(1);
        *blocked_count == 1
    }

    // upstream: intel_hotplug.c unblock_hpd_pin()
    fn unblock_hpd_pin<I: HotplugIo>(&mut self, io: &mut I, pin: HpdPin) -> bool {
        let blocked_count = &mut self.hotplug.stats[pin as usize].blocked_count;
        if *blocked_count == 0 {
            io.trace(HotplugLog::BlockedPinUnderflow { pin });
            return true;
        }
        *blocked_count -= 1;
        *blocked_count == 0
    }

    // upstream: intel_hotplug.c intel_hpd_block()
    pub fn intel_hpd_block<I: HotplugIo>(&mut self, io: &mut I, encoder: usize) {
        let pin = self.encoders[encoder].hpd_pin;
        if pin == HPD_NONE {
            return;
        }
        io.irq_lock_irq();
        let do_flush = self.block_hpd_pin(pin);
        io.irq_unlock_irq();
        if do_flush && self.hpd_pin_has_pulse(pin) {
            io.flush_work(Work::DigPort);
        }
    }

    // upstream: intel_hotplug.c intel_hpd_unblock()
    pub fn intel_hpd_unblock<I: HotplugIo>(&mut self, io: &mut I, encoder: usize) {
        let pin = self.encoders[encoder].hpd_pin;
        if pin == HPD_NONE {
            return;
        }
        io.irq_lock_irq();
        if self.unblock_hpd_pin(io, pin) {
            self.queue_work_for_missed_irqs(io);
        }
        io.irq_unlock_irq();
    }

    // upstream: intel_hotplug.c intel_hpd_clear_and_unblock()
    pub fn intel_hpd_clear_and_unblock<I: HotplugIo>(&mut self, io: &mut I, encoder: usize) {
        let pin = self.encoders[encoder].hpd_pin;
        if pin == HPD_NONE {
            return;
        }
        io.irq_lock_irq();
        if self.unblock_hpd_pin(io, pin) {
            let bit = pin_bit(pin);
            self.hotplug.event_bits &= !bit;
            self.hotplug.retry_bits &= !bit;
            self.hotplug.short_hpd_pin_mask &= !bit;
            self.hotplug.long_hpd_pin_mask &= !bit;
        }
        io.irq_unlock_irq();
    }

    // upstream: intel_hotplug.c intel_hpd_enable_detection_work()
    pub fn intel_hpd_enable_detection_work<I: HotplugIo>(&mut self, io: &mut I) {
        io.irq_lock_irq();
        self.hotplug.detection_work_enabled = true;
        self.queue_work_for_missed_irqs(io);
        io.irq_unlock_irq();
    }

    // upstream: intel_hotplug.c intel_hpd_disable_detection_work()
    pub fn intel_hpd_disable_detection_work<I: HotplugIo>(&mut self, io: &mut I) {
        io.irq_lock_irq();
        self.hotplug.detection_work_enabled = false;
        io.irq_unlock_irq();
        self.cancel_all_detection_work(io);
    }

    // upstream: intel_hotplug.c intel_hpd_schedule_detection()
    pub fn intel_hpd_schedule_detection<I: HotplugIo>(&mut self, io: &mut I) -> bool {
        io.irq_lock_irqsave();
        let ret = self.queue_delayed_detection_work(io, Work::Hotplug, 0);
        io.irq_unlock_irqrestore();
        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_ports_use_the_source_hpd_pin_numbering() {
        assert_eq!(IntelHotplugDisplay::intel_hpd_pin_default(PORT_A), 4);
        assert_eq!(IntelHotplugDisplay::intel_hpd_pin_default(PORT_A + 1), 5);
        assert_eq!(IntelHotplugDisplay::intel_hpd_pin_default(PORT_A + 5), 9);
    }

    #[test]
    fn hotplug_defaults_keep_storm_threshold_and_disabled_polling() {
        let state = IntelHotplugDisplay::default();
        assert_eq!(
            state.hotplug.hpd_storm_threshold,
            HPD_STORM_DEFAULT_THRESHOLD
        );
        assert!(!state.hotplug.detection_work_enabled);
        assert!(!state.hotplug.poll_enabled);
        assert_eq!(state.hotplug.event_bits, 0);
        assert_eq!(state.hotplug.retry_bits, 0);
    }
}
