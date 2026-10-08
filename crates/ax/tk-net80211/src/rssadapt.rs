//! RSSI-threshold legacy rate adaptation from OpenBSD net80211.
//!
//! Translated from OpenBSD `sys/net80211/ieee80211_rssadapt.c` rev 1.11 and
//! `ieee80211_rssadapt.h` rev 1.5 (BSD-3-Clause). Copyright (c) 2003, 2004
//! David Young.

pub const RSSADAPT_BUCKETS: usize = 3;
pub const RSSADAPT_BUCKET0: usize = 128;
pub const RSSADAPT_BUCKET_POWER: u32 = 3;
pub const RATE_SIZE: usize = 8;
pub const RATE_BASIC: u8 = 0x80;
pub const RATE_VALUE: u8 = 0x7f;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyRateSet {
    pub rates: [u8; RATE_SIZE],
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RssDescriptor {
    pub length: usize,
    pub rate_index: usize,
    pub rssi: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RssAdapt {
    /// Exponential-average RSSI in Q8 format.
    pub average_rssi: u16,
    pub failed_packets: u32,
    pub successful_packets: u32,
    pub packet_rate: u32,
    pub rate_thresholds: [[u16; RATE_SIZE]; RSSADAPT_BUCKETS],
    pub last_raise_us: u64,
    pub raise_interval_us: u64,
}

impl Default for RssAdapt {
    fn default() -> Self {
        Self {
            average_rssi: 0,
            failed_packets: 0,
            successful_packets: 0,
            packet_rate: 0,
            rate_thresholds: [[0; RATE_SIZE]; RSSADAPT_BUCKETS],
            last_raise_us: 0,
            raise_interval_us: 0,
        }
    }
}

fn bucket_index(length: usize) -> usize {
    let mut top = RSSADAPT_BUCKET0;
    for index in 0..RSSADAPT_BUCKETS {
        if length <= top {
            return index;
        }
        top <<= RSSADAPT_BUCKET_POWER;
    }
    RSSADAPT_BUCKETS - 1
}

fn interpolate(denominator: u32, old_weight: u32, old: u16, new: u32) -> u16 {
    ((old_weight * u32::from(old) + (denominator - old_weight) * new) / denominator) as u16
}

/// Choose the highest basic-capable rate whose frame-length RSS threshold is met.
// upstream: ieee80211_rssadapt.c ieee80211_rssadapt_choose()
pub fn choose_rate(
    adapt: &RssAdapt,
    rates: &LegacyRateSet,
    is_control_frame: bool,
    length: usize,
    fixed_rate: Option<usize>,
    do_not_adapt: bool,
) -> usize {
    let mut flags = if is_control_frame { RATE_BASIC } else { 0 };
    let thresholds = &adapt.rate_thresholds[bucket_index(length)];
    let mut rate_index = 0usize;
    let mut index = if let Some(fixed) = fixed_rate {
        if fixed < rates.count && rates.rates[fixed] & flags == flags {
            return fixed;
        }
        flags |= RATE_BASIC;
        fixed as isize
    } else {
        rates.count as isize
    };

    while index > 0 {
        index -= 1;
        rate_index = index as usize;
        if rate_index >= rates.count || rates.rates[rate_index] & flags != flags {
            continue;
        }
        if do_not_adapt || thresholds[rate_index] < adapt.average_rssi {
            break;
        }
    }
    rate_index
}

/// Update the smoothed packet rate and source 0.1-to-10 second raise interval.
// upstream: ieee80211_rssadapt.c ieee80211_rssadapt_updatestats()
pub fn update_stats(adapt: &mut RssAdapt) {
    adapt.packet_rate =
        (adapt.packet_rate + 10 * (adapt.failed_packets + adapt.successful_packets)) / 2;
    adapt.failed_packets = 0;
    adapt.successful_packets = 0;
    let interval = (10_000_000u32 / (10 * adapt.packet_rate).max(1)).max(100_000);
    adapt.raise_interval_us = u64::from(interval);
}

/// Incorporate a received RSSI sample with the source 4/8 exponential weight.
// upstream: ieee80211_rssadapt.c ieee80211_rssadapt_input()
pub fn input_rssi(adapt: &mut RssAdapt, rssi: u8) {
    adapt.average_rssi = interpolate(8, 4, adapt.average_rssi, u32::from(rssi) << 8);
}

/// Raise the threshold for a rate/length bucket after a failed transmission.
// upstream: ieee80211_rssadapt.c ieee80211_rssadapt_lower_rate()
pub fn lower_rate(adapt: &mut RssAdapt, rates: &LegacyRateSet, descriptor: RssDescriptor) {
    adapt.failed_packets = adapt.failed_packets.saturating_add(1);
    if descriptor.rate_index >= rates.count || descriptor.rate_index >= RATE_SIZE {
        return;
    }
    let bucket = bucket_index(descriptor.length);
    let threshold = &mut adapt.rate_thresholds[bucket][descriptor.rate_index];
    *threshold = interpolate(8, 4, *threshold, u32::from(descriptor.rssi) << 8);
}

/// Decay the next-rate threshold at eligible packet intervals after success.
// upstream: ieee80211_rssadapt.c ieee80211_rssadapt_raise_rate()
pub fn raise_rate(
    adapt: &mut RssAdapt,
    rates: &LegacyRateSet,
    descriptor: RssDescriptor,
    now_us: u64,
) -> bool {
    adapt.successful_packets = adapt.successful_packets.saturating_add(1);
    if now_us.saturating_sub(adapt.last_raise_us) < adapt.raise_interval_us {
        return false;
    }
    adapt.last_raise_us = now_us;
    if descriptor.rate_index >= rates.count.saturating_sub(1)
        || descriptor.rate_index >= RATE_SIZE - 1
    {
        return false;
    }
    let bucket = bucket_index(descriptor.length);
    let thresholds = &mut adapt.rate_thresholds[bucket];
    let current = descriptor.rate_index;
    let next = current + 1;
    if thresholds[next] <= thresholds[current] {
        return false;
    }
    let replacement = if thresholds[current] == 0 {
        adapt.average_rssi
    } else {
        thresholds[current]
    };
    thresholds[next] = interpolate(16, 15, thresholds[next], u32::from(replacement));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates() -> LegacyRateSet {
        LegacyRateSet {
            rates: [0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24],
            count: 8,
        }
    }

    #[test]
    fn rate_choice_uses_frame_length_bucket_and_basic_rate_constraints() {
        let mut adapt = RssAdapt::default();
        adapt.average_rssi = 40 << 8;
        adapt.rate_thresholds[0] = [0; RATE_SIZE];
        adapt.rate_thresholds[1] = [0; RATE_SIZE];
        adapt.rate_thresholds[2] = [0; RATE_SIZE];
        adapt.rate_thresholds[0][7] = 50 << 8;
        for rate_index in 3..RATE_SIZE {
            adapt.rate_thresholds[1][rate_index] = 50 << 8;
        }
        adapt.rate_thresholds[1][2] = 0;
        assert_eq!(choose_rate(&adapt, &rates(), false, 64, None, true), 7);
        assert_eq!(choose_rate(&adapt, &rates(), true, 64, None, true), 3);
        assert_eq!(choose_rate(&adapt, &rates(), false, 129, None, false), 2);
        assert_eq!(choose_rate(&adapt, &rates(), false, 64, Some(7), true), 7);
    }

    #[test]
    fn rssi_stats_lowering_and_raise_decay_follow_source_weights() {
        let mut adapt = RssAdapt::default();
        input_rssi(&mut adapt, 60);
        assert_eq!(adapt.average_rssi, 30 << 8);
        lower_rate(
            &mut adapt,
            &rates(),
            RssDescriptor {
                length: 100,
                rate_index: 3,
                rssi: 50,
            },
        );
        assert_eq!(adapt.failed_packets, 1);
        assert_eq!(adapt.rate_thresholds[0][3], 25 << 8);
        adapt.rate_thresholds[0][3] = 30 << 8;
        adapt.rate_thresholds[0][4] = 60 << 8;
        assert!(raise_rate(
            &mut adapt,
            &rates(),
            RssDescriptor {
                length: 100,
                rate_index: 3,
                rssi: 0,
            },
            100_000,
        ));
        assert_eq!(
            adapt.rate_thresholds[0][4],
            ((15u32 * (60u32 << 8) + (30u32 << 8)) / 16) as u16
        );
        adapt.failed_packets = 2;
        adapt.successful_packets = 3;
        update_stats(&mut adapt);
        assert_eq!(adapt.packet_rate, 25);
        assert_eq!(adapt.failed_packets, 0);
        assert_eq!(adapt.successful_packets, 0);
        assert_eq!(adapt.raise_interval_us, 100_000);
    }
}
