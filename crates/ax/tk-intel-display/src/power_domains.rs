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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerDomainError {
    MapChanged,
    UnbalancedPut(PowerDomain),
    RefcountOverflow,
    Backend(Error),
    RollbackFailed { operation: Error, rollback: Error },
}

/// Hardware backend corresponding to `intel_power_well_{get,put,is_enabled}`.
pub trait PowerDomainIo {
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
                                if was_zero
                                    && let Err(rollback) =
                                        io.disable_well(previous_group, previous_instance)
                                {
                                    return Err(PowerDomainError::RollbackFailed {
                                        operation: error,
                                        rollback,
                                    });
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
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::{dmc::DmcPlatform, power_map::power_wells};

    #[derive(Default)]
    struct FakePower {
        enabled: Vec<&'static str>,
        disabled: Vec<&'static str>,
    }

    impl PowerDomainIo for FakePower {
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
}
