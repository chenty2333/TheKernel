// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_display_power.c.
// Copyright © 2022 Intel Corporation. Full grant: ../LICENSE-MIT.
//! Synchronous i915 display power-domain reference accounting.
//!
//! The caller serializes access and owns any runtime-PM wake reference. The
//! callback maps each static well descriptor to its generation-specific
//! hardware operation; reference edges alone invoke those callbacks.

use alloc::vec::Vec;

use crate::{
    Error,
    power_map::{DomainList, PowerDomain, PowerWellGroup, PowerWellInstance},
};

/// Return the source-compatible diagnostic name for a display power domain.
// upstream: intel_display_power.c intel_display_power_domain_str()
pub const fn power_domain_name(domain: PowerDomain) -> &'static str {
    match domain {
        PowerDomain::DisplayCore => "DISPLAY_CORE",
        PowerDomain::PipeA => "PIPE_A",
        PowerDomain::PipeB => "PIPE_B",
        PowerDomain::PipeC => "PIPE_C",
        PowerDomain::PipeD => "PIPE_D",
        PowerDomain::PipePanelFitterA => "PIPE_PANEL_FITTER_A",
        PowerDomain::PipePanelFitterB => "PIPE_PANEL_FITTER_B",
        PowerDomain::PipePanelFitterC => "PIPE_PANEL_FITTER_C",
        PowerDomain::PipePanelFitterD => "PIPE_PANEL_FITTER_D",
        PowerDomain::TranscoderA => "TRANSCODER_A",
        PowerDomain::TranscoderB => "TRANSCODER_B",
        PowerDomain::TranscoderC => "TRANSCODER_C",
        PowerDomain::TranscoderD => "TRANSCODER_D",
        PowerDomain::TranscoderEdp => "TRANSCODER_EDP",
        PowerDomain::TranscoderDsiA => "TRANSCODER_DSI_A",
        PowerDomain::TranscoderDsiC => "TRANSCODER_DSI_C",
        PowerDomain::TranscoderVdscPw2 => "TRANSCODER_VDSC_PW2",
        PowerDomain::PortDdiLanesA => "PORT_DDI_LANES_A",
        PowerDomain::PortDdiLanesB => "PORT_DDI_LANES_B",
        PowerDomain::PortDdiLanesC => "PORT_DDI_LANES_C",
        PowerDomain::PortDdiLanesD => "PORT_DDI_LANES_D",
        PowerDomain::PortDdiLanesE => "PORT_DDI_LANES_E",
        PowerDomain::PortDdiLanesF => "PORT_DDI_LANES_F",
        PowerDomain::PortDdiLanesTc1 => "PORT_DDI_LANES_TC1",
        PowerDomain::PortDdiLanesTc2 => "PORT_DDI_LANES_TC2",
        PowerDomain::PortDdiLanesTc3 => "PORT_DDI_LANES_TC3",
        PowerDomain::PortDdiLanesTc4 => "PORT_DDI_LANES_TC4",
        PowerDomain::PortDdiLanesTc5 => "PORT_DDI_LANES_TC5",
        PowerDomain::PortDdiLanesTc6 => "PORT_DDI_LANES_TC6",
        PowerDomain::PortDdiIoA => "PORT_DDI_IO_A",
        PowerDomain::PortDdiIoB => "PORT_DDI_IO_B",
        PowerDomain::PortDdiIoC => "PORT_DDI_IO_C",
        PowerDomain::PortDdiIoD => "PORT_DDI_IO_D",
        PowerDomain::PortDdiIoE => "PORT_DDI_IO_E",
        PowerDomain::PortDdiIoF => "PORT_DDI_IO_F",
        PowerDomain::PortDdiIoTc1 => "PORT_DDI_IO_TC1",
        PowerDomain::PortDdiIoTc2 => "PORT_DDI_IO_TC2",
        PowerDomain::PortDdiIoTc3 => "PORT_DDI_IO_TC3",
        PowerDomain::PortDdiIoTc4 => "PORT_DDI_IO_TC4",
        PowerDomain::PortDdiIoTc5 => "PORT_DDI_IO_TC5",
        PowerDomain::PortDdiIoTc6 => "PORT_DDI_IO_TC6",
        PowerDomain::PortDsi => "PORT_DSI",
        PowerDomain::PortCrt => "PORT_CRT",
        PowerDomain::PortOther => "PORT_OTHER",
        PowerDomain::Vga => "VGA",
        PowerDomain::AudioMmio => "AUDIO_MMIO",
        PowerDomain::AudioPlayback => "AUDIO_PLAYBACK",
        PowerDomain::AuxIoA => "AUX_IO_A",
        PowerDomain::AuxIoB => "AUX_IO_B",
        PowerDomain::AuxIoC => "AUX_IO_C",
        PowerDomain::AuxIoD => "AUX_IO_D",
        PowerDomain::AuxIoE => "AUX_IO_E",
        PowerDomain::AuxIoF => "AUX_IO_F",
        PowerDomain::AuxA => "AUX_A",
        PowerDomain::AuxB => "AUX_B",
        PowerDomain::AuxC => "AUX_C",
        PowerDomain::AuxD => "AUX_D",
        PowerDomain::AuxE => "AUX_E",
        PowerDomain::AuxF => "AUX_F",
        PowerDomain::AuxUsbc1 => "AUX_USBC1",
        PowerDomain::AuxUsbc2 => "AUX_USBC2",
        PowerDomain::AuxUsbc3 => "AUX_USBC3",
        PowerDomain::AuxUsbc4 => "AUX_USBC4",
        PowerDomain::AuxUsbc5 => "AUX_USBC5",
        PowerDomain::AuxUsbc6 => "AUX_USBC6",
        PowerDomain::AuxTbt1 => "AUX_TBT1",
        PowerDomain::AuxTbt2 => "AUX_TBT2",
        PowerDomain::AuxTbt3 => "AUX_TBT3",
        PowerDomain::AuxTbt4 => "AUX_TBT4",
        PowerDomain::AuxTbt5 => "AUX_TBT5",
        PowerDomain::AuxTbt6 => "AUX_TBT6",
        PowerDomain::Gmbus => "GMBUS",
        PowerDomain::GtIrq => "GT_IRQ",
        PowerDomain::DcOff => "DC_OFF",
        PowerDomain::TcColdOff => "TC_COLD_OFF",
        PowerDomain::Init => "INIT",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerDomainError {
    MapChanged,
    UnbalancedPut(PowerDomain),
    RefcountOverflow,
    Backend(Error),
    RollbackFailed { operation: Error, rollback: Error },
    AsyncPutPending(PowerDomain),
    AsyncPutAlreadyPending(PowerDomain),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsyncPutAction {
    /// A new delayed work item must be armed with this delay.
    Arm { delay_ms: u32 },
    /// Existing work is already armed and will consume the queued domain.
    ExistingWork,
    /// No delayed work remains after completing or flushing a batch.
    Idle,
}

/// Hardware backend corresponding to `intel_power_well_{get,put,is_enabled}`.
pub trait PowerDomainIo {
    fn sync_well(
        &mut self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
        use_count: u32,
    ) -> Result<(), Error>;
    fn enable_well(
        &mut self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
    ) -> Result<(), Error>;
    fn disable_well(
        &mut self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
    ) -> Result<(), Error>;
    fn well_is_enabled(
        &self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
    ) -> Result<bool, Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PowerDomainState {
    map_identity: usize,
    map_len: usize,
    domain_counts: Vec<(PowerDomain, u32)>,
    well_counts: Vec<u32>,
    async_put_domains: [Vec<PowerDomain>; 2],
    async_put_armed: Option<u32>,
    async_put_next_delay_ms: u32,
}

fn flat_well_count(map: &[PowerWellGroup]) -> usize {
    map.iter().map(|group| group.instances.len()).sum()
}

fn has_domain(instance: PowerWellInstance, domain: PowerDomain) -> bool {
    match instance.domains {
        DomainList::All => true,
        DomainList::None => false,
        DomainList::Set(domains) => domains.contains(&domain),
    }
}

fn domain_count_mut(counts: &mut Vec<(PowerDomain, u32)>, domain: PowerDomain) -> &mut u32 {
    let index = counts
        .iter()
        .position(|(candidate, _)| *candidate == domain)
        .unwrap_or_else(|| {
            counts.push((domain, 0));
            counts.len() - 1
        });
    &mut counts[index].1
}

impl PowerDomainState {
    /// Create counters for one immutable platform map.
    pub fn new(map: &'static [PowerWellGroup]) -> Self {
        Self {
            map_identity: map.as_ptr() as usize,
            map_len: map.len(),
            domain_counts: Vec::new(),
            well_counts: alloc::vec![0; flat_well_count(map)],
            async_put_domains: [Vec::new(), Vec::new()],
            async_put_armed: None,
            async_put_next_delay_ms: 0,
        }
    }

    fn validate_map(&self, map: &[PowerWellGroup]) -> Result<(), PowerDomainError> {
        if self.map_identity != map.as_ptr() as usize
            || self.map_len != map.len()
            || self.well_counts.len() != flat_well_count(map)
        {
            return Err(PowerDomainError::MapChanged);
        }
        Ok(())
    }

    pub fn domain_use_count(&self, domain: PowerDomain) -> u32 {
        self.domain_counts
            .iter()
            .find(|(candidate, _)| *candidate == domain)
            .map_or(0, |(_, count)| *count)
    }

    /// Synchronize the request state for every well owned by one domain.
    // upstream: intel_display_power.c intel_power_domains_sync_hw()
    pub fn sync_domain(
        &self,
        map: &[PowerWellGroup],
        domain: PowerDomain,
        io: &mut impl PowerDomainIo,
    ) -> Result<(), PowerDomainError> {
        self.validate_map(map)?;
        let mut flat = 0;
        for group in map {
            for instance in group.instances.iter().copied() {
                if has_domain(instance, domain) {
                    io.sync_well(*group, instance, self.well_counts[flat])
                        .map_err(PowerDomainError::Backend)?;
                }
                flat += 1;
            }
        }
        Ok(())
    }

    pub fn well_use_count(
        &self,
        map: &[PowerWellGroup],
        group_index: usize,
        instance_index: usize,
    ) -> Result<u32, PowerDomainError> {
        self.validate_map(map)?;
        if group_index >= map.len() {
            return Err(PowerDomainError::MapChanged);
        }
        let flat = map[..group_index]
            .iter()
            .map(|group| group.instances.len())
            .sum::<usize>()
            .checked_add(instance_index)
            .ok_or(PowerDomainError::MapChanged)?;
        self.well_counts
            .get(flat)
            .copied()
            .ok_or(PowerDomainError::MapChanged)
    }

    /// Test all non-always-on wells mapped to the domain, as i915 does.
    // upstream: intel_display_power.c __intel_display_power_is_enabled()
    pub fn is_enabled(
        &self,
        map: &[PowerWellGroup],
        domain: PowerDomain,
        runtime_active: bool,
        io: &impl PowerDomainIo,
    ) -> Result<bool, PowerDomainError> {
        self.validate_map(map)?;
        if !runtime_active {
            return Ok(false);
        }
        for group in map.iter().rev() {
            for instance in group.instances.iter().rev().copied() {
                if has_domain(instance, domain)
                    && !instance.always_on
                    && !io
                        .well_is_enabled(*group, instance)
                        .map_err(PowerDomainError::Backend)?
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Acquire a domain and its wells in i915 map order.
    // upstream: intel_display_power.c __intel_display_power_get_domain()
    pub fn get(
        &mut self,
        map: &[PowerWellGroup],
        domain: PowerDomain,
        io: &mut impl PowerDomainIo,
    ) -> Result<(), PowerDomainError> {
        self.validate_map(map)?;
        if self.take_async_put(domain) {
            return Ok(());
        }
        let domain_next = self
            .domain_use_count(domain)
            .checked_add(1)
            .ok_or(PowerDomainError::RefcountOverflow)?;
        let mut flat = 0;
        for group in map {
            for instance in group.instances {
                if has_domain(*instance, domain) && self.well_counts[flat] == u32::MAX {
                    return Err(PowerDomainError::RefcountOverflow);
                }
                flat += 1;
            }
        }
        let mut processed = Vec::new();
        flat = 0;
        for group in map {
            for instance in group.instances.iter().copied() {
                if has_domain(instance, domain) {
                    let count = self.well_counts[flat];
                    if count == 0 {
                        if let Err(error) = io.enable_well(*group, instance) {
                            for (previous_flat, previous_group, previous_instance, was_zero) in
                                processed.into_iter().rev()
                            {
                                self.well_counts[previous_flat] -= 1;
                                if was_zero {
                                    if let Err(rollback) =
                                        io.disable_well(previous_group, previous_instance)
                                    {
                                        return Err(PowerDomainError::RollbackFailed {
                                            operation: error,
                                            rollback,
                                        });
                                    }
                                }
                            }
                            return Err(PowerDomainError::Backend(error));
                        }
                    }
                    self.well_counts[flat] = count
                        .checked_add(1)
                        .ok_or(PowerDomainError::RefcountOverflow)?;
                    processed.push((flat, *group, instance, count == 0));
                }
                flat += 1;
            }
        }
        *domain_count_mut(&mut self.domain_counts, domain) = domain_next;
        Ok(())
    }

    /// Release a domain and its wells in reverse map order.
    // upstream: intel_display_power.c __intel_display_power_put_domain()
    pub fn put(
        &mut self,
        map: &[PowerWellGroup],
        domain: PowerDomain,
        io: &mut impl PowerDomainIo,
    ) -> Result<(), PowerDomainError> {
        self.validate_map(map)?;
        if self.async_put_pending(domain) {
            return Err(PowerDomainError::AsyncPutPending(domain));
        }
        if self.domain_use_count(domain) == 0 {
            return Err(PowerDomainError::UnbalancedPut(domain));
        }
        let mut preflight_flat = 0;
        for group in map {
            for instance in group.instances.iter().copied() {
                if has_domain(instance, domain) && self.well_counts[preflight_flat] == 0 {
                    return Err(PowerDomainError::UnbalancedPut(domain));
                }
                preflight_flat += 1;
            }
        }
        let original_counts = self.well_counts.clone();
        let mut disabled = Vec::new();
        let mut flat = self.well_counts.len();
        for group in map.iter().rev() {
            for instance in group.instances.iter().rev().copied() {
                flat -= 1;
                if !has_domain(instance, domain) {
                    continue;
                }
                let count = self.well_counts[flat];
                if count == 1 {
                    if let Err(error) = io.disable_well(*group, instance) {
                        self.well_counts.clone_from(&original_counts);
                        for (previous_group, previous_instance) in disabled.into_iter().rev() {
                            if let Err(rollback) = io.enable_well(previous_group, previous_instance)
                            {
                                return Err(PowerDomainError::RollbackFailed {
                                    operation: error,
                                    rollback,
                                });
                            }
                        }
                        return Err(PowerDomainError::Backend(error));
                    }
                    disabled.push((*group, instance));
                }
                self.well_counts[flat] = count - 1;
            }
        }
        *domain_count_mut(&mut self.domain_counts, domain) -= 1;
        Ok(())
    }

    /// Grab a reference only while the runtime-PM domain is already active.
    // upstream: intel_display_power.c intel_display_power_get_if_enabled()
    pub fn get_if_enabled(
        &mut self,
        map: &[PowerWellGroup],
        domain: PowerDomain,
        runtime_active: bool,
        io: &mut impl PowerDomainIo,
    ) -> Result<bool, PowerDomainError> {
        if !self.is_enabled(map, domain, runtime_active, io)? {
            return Ok(false);
        }
        self.get(map, domain, io)?;
        Ok(true)
    }

    fn async_put_pending(&self, domain: PowerDomain) -> bool {
        self.async_put_domains
            .iter()
            .any(|pending| pending.contains(&domain))
    }

    fn take_async_put(&mut self, domain: PowerDomain) -> bool {
        for pending in &mut self.async_put_domains {
            if let Some(index) = pending.iter().position(|candidate| *candidate == domain) {
                pending.swap_remove(index);
                return true;
            }
        }
        false
    }

    /// Queue a delayed final reference release, or drop a non-final reference now.
    ///
    /// A negative delay selects i915's 100 ms default. The caller owns the
    /// runtime-PM reference and arms work when this returns `Arm`.
    // upstream: intel_display_power.c __intel_display_power_put_async()
    pub fn put_async(
        &mut self,
        map: &[PowerWellGroup],
        domain: PowerDomain,
        delay_ms: i32,
        io: &mut impl PowerDomainIo,
    ) -> Result<AsyncPutAction, PowerDomainError> {
        self.validate_map(map)?;
        let use_count = self.domain_use_count(domain);
        if use_count == 0 {
            return Err(PowerDomainError::UnbalancedPut(domain));
        }
        if use_count > 1 {
            self.put(map, domain, io)?;
            return Ok(AsyncPutAction::Idle);
        }
        if self.async_put_pending(domain) {
            return Err(PowerDomainError::AsyncPutAlreadyPending(domain));
        }

        let delay_ms = if delay_ms >= 0 { delay_ms as u32 } else { 100 };
        if self.async_put_armed.is_some() {
            self.async_put_domains[1].push(domain);
            self.async_put_next_delay_ms = self.async_put_next_delay_ms.max(delay_ms);
            Ok(AsyncPutAction::ExistingWork)
        } else {
            self.async_put_domains[0].push(domain);
            self.async_put_armed = Some(delay_ms);
            Ok(AsyncPutAction::Arm { delay_ms })
        }
    }

    /// Complete the current delayed batch and request a requeue if later puts arrived.
    // upstream: intel_display_power.c intel_display_power_put_async_work()
    pub fn process_async_put_work(
        &mut self,
        map: &[PowerWellGroup],
        io: &mut impl PowerDomainIo,
    ) -> Result<AsyncPutAction, PowerDomainError> {
        self.validate_map(map)?;
        if self.async_put_armed.take().is_none() {
            return Ok(AsyncPutAction::Idle);
        }
        let current = core::mem::take(&mut self.async_put_domains[0]);
        for (index, domain) in current.iter().copied().enumerate() {
            if let Err(error) = self.put(map, domain, io) {
                self.async_put_domains[0].extend_from_slice(&current[index..]);
                self.async_put_armed = Some(0);
                return Err(error);
            }
        }
        if self.async_put_domains[1].is_empty() {
            self.async_put_next_delay_ms = 0;
            return Ok(AsyncPutAction::Idle);
        }

        self.async_put_domains[0] = core::mem::take(&mut self.async_put_domains[1]);
        let delay_ms = core::mem::take(&mut self.async_put_next_delay_ms);
        self.async_put_armed = Some(delay_ms);
        Ok(AsyncPutAction::Arm { delay_ms })
    }

    /// Synchronously release both delayed batches, matching the source flush operation.
    // upstream: intel_display_power.c intel_display_power_flush_work()
    pub fn flush_async_puts(
        &mut self,
        map: &[PowerWellGroup],
        io: &mut impl PowerDomainIo,
    ) -> Result<usize, PowerDomainError> {
        self.validate_map(map)?;
        self.async_put_armed = None;
        self.async_put_next_delay_ms = 0;
        let mut pending = core::mem::take(&mut self.async_put_domains[0]);
        pending.extend(core::mem::take(&mut self.async_put_domains[1]));
        let mut released = 0;
        for (index, domain) in pending.iter().copied().enumerate() {
            if let Err(error) = self.put(map, domain, io) {
                self.async_put_domains[0].extend_from_slice(&pending[index..]);
                self.async_put_armed = Some(0);
                return Err(error);
            }
            released += 1;
        }
        Ok(released)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::{dmc::DmcPlatform, power_map::power_wells};

    #[test]
    fn domain_diagnostic_names_match_i915() {
        assert_eq!(power_domain_name(PowerDomain::DisplayCore), "DISPLAY_CORE");
        assert_eq!(power_domain_name(PowerDomain::PortOther), "PORT_OTHER");
        assert_eq!(power_domain_name(PowerDomain::Gmbus), "GMBUS");
        assert_eq!(power_domain_name(PowerDomain::GtIrq), "GT_IRQ");
        assert_eq!(power_domain_name(PowerDomain::TcColdOff), "TC_COLD_OFF");
    }

    #[derive(Default)]
    struct FakePower {
        synced: Vec<&'static str>,
        enabled: Vec<&'static str>,
        disabled: Vec<&'static str>,
    }

    impl PowerDomainIo for FakePower {
        fn sync_well(
            &mut self,
            _: PowerWellGroup,
            instance: PowerWellInstance,
            _: u32,
        ) -> Result<(), Error> {
            self.synced.push(instance.name);
            Ok(())
        }
        fn enable_well(
            &mut self,
            _: PowerWellGroup,
            instance: PowerWellInstance,
        ) -> Result<(), Error> {
            self.disabled.retain(|name| *name != instance.name);
            self.enabled.push(instance.name);
            Ok(())
        }
        fn disable_well(
            &mut self,
            _: PowerWellGroup,
            instance: PowerWellInstance,
        ) -> Result<(), Error> {
            self.disabled.push(instance.name);
            Ok(())
        }
        fn well_is_enabled(
            &self,
            _: PowerWellGroup,
            instance: PowerWellInstance,
        ) -> Result<bool, Error> {
            Ok(self.enabled.contains(&instance.name) && !self.disabled.contains(&instance.name))
        }
    }

    #[test]
    fn xelpd_pipe_a_refs_only_switch_pwa_on_zero_to_one_edges() {
        let map = power_wells(DmcPlatform::AlderLakeN);
        let mut state = PowerDomainState::new(map);
        let mut io = FakePower::default();
        state.get(map, PowerDomain::PipeA, &mut io).unwrap();
        state.get(map, PowerDomain::PipeA, &mut io).unwrap();
        assert_eq!(state.domain_use_count(PowerDomain::PipeA), 2);
        assert_eq!(io.enabled.iter().filter(|name| **name == "PW_A").count(), 1);
        assert!(
            state
                .is_enabled(map, PowerDomain::PipeA, true, &io)
                .unwrap()
        );
        state.put(map, PowerDomain::PipeA, &mut io).unwrap();
        assert!(!io.disabled.contains(&"PW_A"));
        state.put(map, PowerDomain::PipeA, &mut io).unwrap();
        assert!(io.disabled.contains(&"PW_A"));
        assert_eq!(state.domain_use_count(PowerDomain::PipeA), 0);
        state.get(map, PowerDomain::PipeA, &mut io).unwrap();
        assert_eq!(io.enabled.iter().filter(|name| **name == "PW_A").count(), 2);
        assert!(
            state
                .is_enabled(map, PowerDomain::PipeA, true, &io)
                .unwrap()
        );
    }

    #[test]
    fn sync_domain_visits_only_source_mapped_wells_in_map_order() {
        let map = power_wells(DmcPlatform::AlderLakeN);
        let state = PowerDomainState::new(map);
        let mut io = FakePower::default();
        state.sync_domain(map, PowerDomain::PipeA, &mut io).unwrap();
        assert_eq!(io.synced, ["always-on", "PW_A"]);
    }

    #[test]
    fn async_put_reuses_pending_reference_on_get_and_releases_after_work() {
        let map = power_wells(DmcPlatform::AlderLakeN);
        let mut state = PowerDomainState::new(map);
        let mut io = FakePower::default();
        state.get(map, PowerDomain::PipeA, &mut io).unwrap();
        state.get(map, PowerDomain::PipeA, &mut io).unwrap();
        assert_eq!(
            state
                .put_async(map, PowerDomain::PipeA, 20, &mut io)
                .unwrap(),
            AsyncPutAction::Idle
        );
        assert_eq!(state.domain_use_count(PowerDomain::PipeA), 1);
        assert_eq!(
            state
                .put_async(map, PowerDomain::PipeA, -1, &mut io)
                .unwrap(),
            AsyncPutAction::Arm { delay_ms: 100 }
        );
        // Source `grab_async_put_ref()` cancels the delayed drop and reuses
        // the existing count/well, rather than toggling hardware.
        state.get(map, PowerDomain::PipeA, &mut io).unwrap();
        assert_eq!(state.domain_use_count(PowerDomain::PipeA), 1);
        assert_eq!(io.enabled.iter().filter(|name| **name == "PW_A").count(), 1);
        assert_eq!(
            state
                .put_async(map, PowerDomain::PipeA, 0, &mut io)
                .unwrap(),
            AsyncPutAction::ExistingWork
        );
        assert_eq!(
            state.process_async_put_work(map, &mut io).unwrap(),
            AsyncPutAction::Arm { delay_ms: 0 }
        );
        assert_eq!(
            state.process_async_put_work(map, &mut io).unwrap(),
            AsyncPutAction::Idle
        );
        assert_eq!(state.domain_use_count(PowerDomain::PipeA), 0);
        assert!(io.disabled.contains(&"PW_A"));
    }

    #[test]
    fn async_put_uses_two_batches_and_maximum_requeue_delay() {
        let map = power_wells(DmcPlatform::AlderLakeN);
        let mut state = PowerDomainState::new(map);
        let mut io = FakePower::default();
        state.get(map, PowerDomain::PipeA, &mut io).unwrap();
        state.get(map, PowerDomain::PipeB, &mut io).unwrap();
        assert_eq!(
            state
                .put_async(map, PowerDomain::PipeA, 10, &mut io)
                .unwrap(),
            AsyncPutAction::Arm { delay_ms: 10 }
        );
        assert_eq!(
            state
                .put_async(map, PowerDomain::PipeB, 40, &mut io)
                .unwrap(),
            AsyncPutAction::ExistingWork
        );
        assert_eq!(
            state.process_async_put_work(map, &mut io).unwrap(),
            AsyncPutAction::Arm { delay_ms: 40 }
        );
        assert_eq!(state.domain_use_count(PowerDomain::PipeA), 0);
        assert_eq!(state.domain_use_count(PowerDomain::PipeB), 1);
        assert_eq!(state.flush_async_puts(map, &mut io).unwrap(), 1);
        assert_eq!(state.domain_use_count(PowerDomain::PipeB), 0);
    }
}
