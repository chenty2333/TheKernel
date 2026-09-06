/// Typed result of one endpoint-owned destructive statistics snapshot.
///
/// The Layer 1 packet endpoint remains the sole owner of live counters and
/// resets them exactly once. This value only maps that already-destructive
/// snapshot without inventing event attribution the endpoint did not record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PacketStatistics {
    packets: u64,
    drops: u64,
    filter_rejected: u64,
    filter_errors: u64,
}

impl PacketStatistics {
    /// Maps one endpoint-owned destructive snapshot without taking ownership
    /// of the counters or introducing a second reset point.
    ///
    /// `packets` and `drops` are already the endpoint's Linux-compatible
    /// aggregates. Filter outcomes remain diagnostics and are not added to
    /// either total. The values are copied exactly because the endpoint does
    /// not currently expose drop reasons or a saturation marker.
    pub const fn from_destructive_snapshot(
        packets: u64,
        drops: u64,
        filter_rejected: u64,
        filter_errors: u64,
    ) -> Self {
        Self {
            packets,
            drops,
            filter_rejected,
            filter_errors,
        }
    }

    /// Endpoint-provided Linux-visible packet aggregate, including its drops.
    pub const fn packets(self) -> u64 {
        self.packets
    }

    /// Endpoint-provided Linux-visible drop aggregate.
    pub const fn drops(self) -> u64 {
        self.drops
    }

    /// Packets rejected by an attached filter before admission.
    ///
    /// This diagnostic counter is intentionally excluded from Linux-visible
    /// `packets` and `drops`.
    pub const fn filter_rejected(self) -> u64 {
        self.filter_rejected
    }

    /// Attached-filter executions that returned an internal mechanism error.
    ///
    /// This Layer 1 diagnostic is also excluded from Linux-visible totals.
    pub const fn filter_errors(self) -> u64 {
        self.filter_errors
    }

    /// Returns whether every aggregate and diagnostic counter is zero.
    pub const fn is_empty(self) -> bool {
        self.packets == 0 && self.drops == 0 && self.filter_rejected == 0 && self.filter_errors == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_snapshot_is_mapped_without_reason_invention() {
        let stats = PacketStatistics::from_destructive_snapshot(6, 5, 7, 11);
        assert_eq!(stats.packets(), 6);
        assert_eq!(stats.drops(), 5);
        assert_eq!(stats.filter_rejected(), 7);
        assert_eq!(stats.filter_errors(), 11);
    }

    #[test]
    fn aggregate_values_are_preserved_even_without_reason_decomposition() {
        let stats = PacketStatistics::from_destructive_snapshot(u64::MAX, u64::MAX, 0, 0);
        assert_eq!(stats.packets(), u64::MAX);
        assert_eq!(stats.drops(), u64::MAX);
        assert!(!stats.is_empty());

        assert!(PacketStatistics::from_destructive_snapshot(0, 0, 0, 0).is_empty());
    }
}
