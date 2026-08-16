//! Explicit, capability-only PMU integration for diagnostic kernels.
//!
//! Reading this surface may probe an architecture backend, but it never
//! configures or starts a counter. Hardware samples need a separate explicit
//! session and evidence contract; a capability is not a measurement.

use axpmu::{Capabilities, CounterSource, Event};

const EVENTS: [Event; 5] = [
    Event::CpuCycles,
    Event::Instructions,
    Event::DataTlbReadMisses,
    Event::DataTlbWriteMisses,
    Event::InstructionTlbReadMisses,
];

/// One allocation-free PMU capability observation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PmuCapabilitySnapshot {
    capabilities: Capabilities,
}

impl PmuCapabilitySnapshot {
    fn new(capabilities: Capabilities) -> Self {
        Self { capabilities }
    }

    pub(crate) fn source(self) -> &'static str {
        match self.capabilities.source() {
            CounterSource::Platform => "platform",
            _ => "unknown",
        }
    }

    pub(crate) fn counter_count(self) -> usize {
        self.capabilities.counter_count()
    }

    pub(crate) fn has_consistent_snapshot(self) -> bool {
        self.capabilities.has_consistent_snapshot()
    }

    pub(crate) fn events(self) -> impl Iterator<Item = (&'static str, bool)> {
        let requestable = self.capabilities.requestable_events();
        EVENTS
            .into_iter()
            .map(move |event| (event_name(event), requestable.contains(event)))
    }
}

const fn event_name(event: Event) -> &'static str {
    match event {
        Event::CpuCycles => "cpu_cycles",
        Event::Instructions => "instructions",
        Event::DataTlbReadMisses => "dtlb_read_misses",
        Event::DataTlbWriteMisses => "dtlb_write_misses",
        Event::InstructionTlbReadMisses => "itlb_read_misses",
        _ => "unknown",
    }
}

/// Probes requestable events without reserving or starting hardware counters.
pub(crate) fn capability_snapshot() -> PmuCapabilitySnapshot {
    PmuCapabilitySnapshot::new(Capabilities::unsupported(CounterSource::Platform))
}

#[cfg(test)]
mod tests {
    use axpmu::EventMask;

    use super::*;

    #[test]
    fn typed_capabilities_do_not_claim_samples() {
        let snapshot = PmuCapabilitySnapshot::new(Capabilities::new(
            CounterSource::Platform,
            2,
            EventMask::from_event(Event::CpuCycles)
                .union(EventMask::from_event(Event::Instructions)),
            false,
        ));
        assert_eq!(snapshot.source(), "platform");
        assert_eq!(snapshot.counter_count(), 2);
        assert!(!snapshot.has_consistent_snapshot());
        assert_eq!(
            snapshot.events().collect::<alloc::vec::Vec<_>>(),
            [
                ("cpu_cycles", true),
                ("instructions", true),
                ("dtlb_read_misses", false),
                ("dtlb_write_misses", false),
                ("itlb_read_misses", false),
            ]
        );
    }
}
