//! HT goodput rate adaptation from OpenBSD net80211.
//!
//! Translated from OpenBSD `sys/net80211/ieee80211_ra.c` rev 1.5,
//! `ieee80211_ra.h` rev 1.2, and the 11n rateset table in `ieee80211.c`
//! rev 1.92 (ISC). Copyright (c) 2021 Christian Ehrhardt. Copyright (c)
//! 2016, 2021 Stefan Sperling. Copyright (c) 2016 Theo Buehler. The
//! rateset data in `ieee80211.c` is Copyright (c) 2001 Atsushi Onoe and
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting.

pub const RA_FP_SHIFT: u32 = 21;
pub const RA_FP_ONE: u64 = 1 << RA_FP_SHIFT;
pub const RATE_THRESHOLD: u64 = RA_FP_ONE / 64;
pub const MCS_COUNT: usize = 32;
pub const RA_NOT_PROBING: u32 = 0;
pub const RA_PROBING_DOWN: u32 = 1;
pub const RA_PROBING_UP: u32 = 2;
pub const RA_PROBING_INTER: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HtRateSet {
    pub rates: [u16; 8],
    pub mcs_mask: u32,
    pub min_mcs: u8,
    pub max_mcs: u8,
    pub chan40: bool,
    pub sgi: bool,
}

const fn rateset(rates: [u16; 8], base: u8, chan40: bool, sgi: bool) -> HtRateSet {
    HtRateSet {
        rates,
        mcs_mask: 0xffu32 << base,
        min_mcs: base,
        max_mcs: base + 7,
        chan40,
        sgi,
    }
}

/// OpenBSD's ordered 11n MCS ratesets, in 500-kbit/s units.
pub const HT_RATESETS: [HtRateSet; 16] = [
    rateset([13, 26, 39, 52, 78, 104, 117, 130], 0, false, false),
    rateset([14, 29, 43, 58, 87, 116, 130, 144], 0, false, true),
    rateset([26, 52, 78, 104, 156, 208, 234, 260], 8, false, false),
    rateset([29, 58, 87, 116, 173, 231, 261, 289], 8, false, true),
    rateset([39, 78, 117, 156, 234, 312, 351, 390], 16, false, false),
    rateset([43, 87, 130, 173, 260, 347, 390, 433], 16, false, true),
    rateset([52, 104, 156, 208, 312, 416, 468, 520], 24, false, false),
    rateset([58, 116, 173, 231, 347, 462, 520, 578], 24, false, true),
    rateset([27, 54, 81, 108, 162, 216, 243, 270], 0, true, false),
    rateset([30, 60, 90, 120, 180, 240, 270, 300], 0, true, true),
    rateset([54, 108, 192, 216, 324, 432, 486, 540], 8, true, false),
    rateset([60, 120, 180, 240, 360, 480, 540, 600], 8, true, true),
    rateset([81, 162, 243, 324, 486, 648, 729, 810], 16, true, false),
    rateset([90, 180, 270, 360, 540, 720, 810, 900], 16, true, true),
    rateset([108, 216, 324, 432, 324, 864, 972, 1080], 24, true, false),
    rateset([120, 240, 360, 480, 520, 960, 1080, 1200], 24, true, true),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GoodputStats {
    pub measured: u64,
    pub average: u64,
    pub stddeviation: u64,
    pub loss: u64,
    pub nprobe_pkts: u32,
    pub nprobe_fail: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RaNode {
    pub valid_probes: u32,
    pub valid_rates: u32,
    pub candidate_rates: u32,
    pub probed_rates: u32,
    pub probing: u32,
    pub best_mcs: u8,
    pub g: [GoodputStats; MCS_COUNT],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HtPeer {
    pub tx_mcs: u8,
    pub rx_mcs: u32,
    pub tx_mcs_set: u16,
    pub supported_tx_mcs: u32,
    pub supports_40mhz: bool,
    pub channel_40mhz: bool,
    pub sgi20: bool,
    pub sgi40: bool,
}

#[inline]
fn bit(mcs: u8) -> u32 {
    1u32 << mcs
}

// upstream: ieee80211_ra.c ra_fixedp_split()
pub fn fixedp_split(fp: u64) -> (u32, u32) {
    let integer = (fp >> RA_FP_SHIFT) as u32;
    let fraction = (((fp & (u64::MAX >> (64 - RA_FP_SHIFT))) * 100) >> RA_FP_SHIFT) as u32;
    (integer, fraction)
}

// upstream: ieee80211_ra.c ra_fp_sprintf()
pub fn fixedp_string(fp: u64) -> alloc::string::String {
    let (i, f) = fixedp_split(fp);
    alloc::format!("{i}.{f:02}")
}

// upstream: ieee80211_ra.c ieee80211_ra_get_ht_rateset()
pub fn get_ht_rateset(mcs: u8, chan40: bool, sgi: bool) -> &'static HtRateSet {
    HT_RATESETS
        .iter()
        .find(|r| r.chan40 == chan40 && r.sgi == sgi && (r.min_mcs..=r.max_mcs).contains(&mcs))
        .expect("MCS not in an HT rateset")
}

// upstream: ieee80211_ra.c ieee80211_ra_use_ht_sgi()
pub fn use_ht_sgi(peer: &HtPeer) -> bool {
    if peer.channel_40mhz && peer.supports_40mhz {
        peer.sgi40
    } else {
        peer.sgi20
    }
}

// upstream: ieee80211_ra.c ieee80211_ra_get_txrate()
pub fn get_txrate(mcs: u8, chan40: bool, sgi: bool) -> u64 {
    let rs = get_ht_rateset(mcs, chan40, sgi);
    (u64::from(rs.rates[(mcs - rs.min_mcs) as usize]) << RA_FP_SHIFT) * 500 / 1000
}

// upstream: ieee80211_ra.c ieee80211_ra_next_lower_intra_rate()
pub fn next_lower_intra_rate(rn: &RaNode, peer: &HtPeer) -> u8 {
    let rs = get_ht_rateset(peer.tx_mcs, peer.supports_40mhz, use_ht_sgi(peer));
    if peer.tx_mcs == rs.min_mcs {
        return rs.min_mcs;
    }
    for mcs in (rs.min_mcs..=rs.max_mcs).rev() {
        if rn.valid_rates & bit(mcs) != 0 && mcs < peer.tx_mcs {
            return mcs;
        }
    }
    peer.tx_mcs
}

// upstream: ieee80211_ra.c ieee80211_ra_next_intra_rate()
pub fn next_intra_rate(rn: &RaNode, peer: &HtPeer) -> u8 {
    let rs = get_ht_rateset(peer.tx_mcs, peer.supports_40mhz, use_ht_sgi(peer));
    if peer.tx_mcs == rs.max_mcs {
        return rs.max_mcs;
    }
    for mcs in rs.min_mcs..=rs.max_mcs {
        if rn.valid_rates & bit(mcs) != 0 && mcs > peer.tx_mcs {
            return mcs;
        }
    }
    peer.tx_mcs
}

fn next_rateset_index(current: &HtRateSet, up: bool, chan40: bool, sgi: bool) -> Option<usize> {
    let base = if up {
        match current.max_mcs {
            7 => 8,
            15 => 16,
            23 => 24,
            _ => return None,
        }
    } else {
        match current.min_mcs {
            24 => 16,
            16 => 8,
            8 => 0,
            _ => return None,
        }
    };
    let row = HT_RATESETS
        .iter()
        .position(|r| r.min_mcs == base && r.chan40 == chan40 && r.sgi == sgi)?;
    Some(row)
}

// upstream: ieee80211_ra.c ieee80211_ra_next_rateset()
pub fn next_rateset(rn: &RaNode, peer: &HtPeer) -> Option<&'static HtRateSet> {
    let chan40 = peer.supports_40mhz;
    let sgi = use_ht_sgi(peer);
    let up = rn.probing & RA_PROBING_UP != 0;
    if !up && rn.probing & RA_PROBING_DOWN == 0 {
        panic!("invalid probing mode {}", rn.probing);
    }
    let next = next_rateset_index(get_ht_rateset(peer.tx_mcs, chan40, sgi), up, chan40, sgi)?;
    let rs = &HT_RATESETS[next];
    if rs.mcs_mask & rn.valid_rates == 0 {
        None
    } else {
        Some(rs)
    }
}

// upstream: ieee80211_ra.c ieee80211_ra_best_mcs_in_rateset()
pub fn best_mcs_in_rateset(rn: &RaNode, rs: &HtRateSet) -> u8 {
    let mut gmax = 0;
    let mut best = rs.min_mcs;
    for mcs in rs.min_mcs..=rs.max_mcs {
        let g = rn.g[mcs as usize];
        if rn.valid_rates & bit(mcs) == 0 {
            continue;
        }
        if g.measured > gmax + RATE_THRESHOLD {
            gmax = g.measured;
            best = mcs;
        }
    }
    best
}

// upstream: ieee80211_ra.c ieee80211_ra_probe_next_rateset()
pub fn probe_next_rateset(rn: &mut RaNode, peer: &mut HtPeer, next: &HtRateSet) {
    let rs = get_ht_rateset(peer.tx_mcs, peer.supports_40mhz, use_ht_sgi(peer));
    let best = best_mcs_in_rateset(rn, rs);
    peer.tx_mcs = next.min_mcs;
    if rn.valid_rates & bit(next.min_mcs) == 0 {
        peer.tx_mcs = next_intra_rate(rn, peer);
    }
    let measured = rn.g[best as usize].measured;
    let mut i = 0;
    for (offset, raw_rate) in next.rates.iter().enumerate() {
        let mcs = next.min_mcs + offset as u8;
        if rn.valid_rates & bit(mcs) == 0 {
            i += 1;
            continue;
        }
        let txrate = (u64::from(*raw_rate) * 500 << RA_FP_SHIFT) / 1000;
        if txrate > measured + RATE_THRESHOLD {
            peer.tx_mcs = mcs;
            break;
        }
        i += 1;
    }
    if i == next.rates.len() {
        peer.tx_mcs = next.max_mcs;
    }
    rn.candidate_rates |= bit(peer.tx_mcs);
    if rn.probing & RA_PROBING_UP != 0 {
        rn.candidate_rates |= bit(next_intra_rate(rn, peer));
    } else if rn.probing & RA_PROBING_DOWN != 0 {
        rn.candidate_rates |= bit(next_lower_intra_rate(rn, peer));
    } else {
        panic!("invalid probing mode {}", rn.probing);
    }
}

// upstream: ieee80211_ra.c ieee80211_ra_next_mcs()
pub fn next_mcs(rn: &RaNode, peer: &HtPeer) -> u8 {
    if rn.probing & RA_PROBING_DOWN != 0 {
        next_lower_intra_rate(rn, peer)
    } else if rn.probing & RA_PROBING_UP != 0 {
        next_intra_rate(rn, peer)
    } else {
        panic!("invalid probing mode {}", rn.probing)
    }
}

// upstream: ieee80211_ra.c ieee80211_ra_probe_clear()
pub fn probe_clear(rn: &mut RaNode, peer: &HtPeer) {
    let g = &mut rn.g[peer.tx_mcs as usize];
    g.nprobe_pkts = 0;
    g.nprobe_fail = 0;
}

// upstream: ieee80211_ra.c ieee80211_ra_probe_done()
pub fn probe_done(rn: &mut RaNode) {
    rn.probing = RA_NOT_PROBING;
    rn.probed_rates = 0;
    rn.valid_probes = 0;
    rn.candidate_rates = 0;
}

// upstream: ieee80211_ra.c ieee80211_ra_intra_mode_ra_finished()
pub fn intra_mode_ra_finished(rn: &mut RaNode, peer: &mut HtPeer) -> bool {
    let g = rn.g[peer.tx_mcs as usize];
    rn.probed_rates |= bit(peer.tx_mcs);
    let rs = get_ht_rateset(peer.tx_mcs, peer.supports_40mhz, use_ht_sgi(peer));
    if rn.probing & RA_PROBING_DOWN != 0 {
        if peer.tx_mcs == rs.min_mcs || rn.probed_rates & bit(rs.min_mcs) != 0 {
            trigger_next_rateset(rn, peer);
            return true;
        }
    } else if rn.probing & RA_PROBING_UP != 0 {
        if peer.tx_mcs == rs.max_mcs || rn.probed_rates & bit(rs.max_mcs) != 0 {
            trigger_next_rateset(rn, peer);
            return true;
        }
    }
    let next = next_mcs(rn, peer);
    if next == peer.tx_mcs {
        trigger_next_rateset(rn, peer);
        return true;
    }
    let rate = get_txrate(next, peer.supports_40mhz, use_ht_sgi(peer));
    if g.loss == 0 && g.measured >= rate + RATE_THRESHOLD {
        trigger_next_rateset(rn, peer);
        return true;
    }
    let best = best_mcs_in_rateset(rn, rs);
    if best != peer.tx_mcs && rn.probed_rates & bit(best) != 0 {
        if (rn.probing & RA_PROBING_UP != 0 && best < peer.tx_mcs)
            || (rn.probing & RA_PROBING_DOWN != 0 && best > peer.tx_mcs)
        {
            trigger_next_rateset(rn, peer);
            return true;
        }
    }
    if rn.candidate_rates & rn.probed_rates == rn.candidate_rates {
        rn.probing &= !RA_PROBING_INTER;
        return true;
    }
    false
}

// upstream: ieee80211_ra.c ieee80211_ra_trigger_next_rateset()
pub fn trigger_next_rateset(rn: &mut RaNode, peer: &mut HtPeer) {
    if let Some(next) = next_rateset(rn, peer).copied() {
        probe_next_rateset(rn, peer, &next);
        rn.probing |= RA_PROBING_INTER;
    } else {
        rn.probing &= !RA_PROBING_INTER;
    }
}

// upstream: ieee80211_ra.c ieee80211_ra_inter_mode_ra_finished()
pub fn inter_mode_ra_finished(rn: &RaNode, _peer: &HtPeer) -> bool {
    rn.probing & RA_PROBING_INTER == 0
}

// upstream: ieee80211_ra.c ieee80211_ra_best_rate()
pub fn best_rate(rn: &RaNode) -> u8 {
    let mut best = rn.best_mcs;
    let mut gmax = rn.g[best as usize].measured;
    for i in 0..MCS_COUNT {
        if rn.valid_rates & (1 << i) == 0 {
            continue;
        }
        let g = rn.g[i].measured;
        if g > gmax + RATE_THRESHOLD {
            gmax = g;
            best = i as u8;
        }
    }
    best
}

// upstream: ieee80211_ra.c ieee80211_ra_probe_next_rate()
pub fn probe_next_rate(rn: &mut RaNode, peer: &mut HtPeer) {
    rn.probed_rates |= bit(peer.tx_mcs);
    peer.tx_mcs = next_mcs(rn, peer);
}

// upstream: ieee80211_ra.c ieee80211_ra_valid_tx_mcs()
pub fn valid_tx_mcs(peer: &HtPeer, mcs: u8) -> bool {
    if peer.tx_mcs_set & (1 << 1) == 0 {
        return peer.supported_tx_mcs & bit(mcs) != 0;
    }
    let streams = 1 + ((peer.tx_mcs_set & 0x000c) >> 2) as u8;
    let max = [7, 15, 23, 31]
        .get(streams.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(0);
    mcs <= max && peer.supported_tx_mcs & bit(mcs) != 0
}

// upstream: ieee80211_ra.c ieee80211_ra_valid_rates()
pub fn valid_rates(peer: &HtPeer) -> u32 {
    let mut valid = 0;
    for mcs in 0..MCS_COUNT {
        if peer.rx_mcs & (1 << mcs) != 0 && valid_tx_mcs(peer, mcs as u8) {
            valid |= 1 << mcs;
        }
    }
    valid
}

// upstream: ieee80211_ra.c ieee80211_ra_probe_valid()
pub fn probe_valid(g: &GoodputStats) -> bool {
    g.nprobe_pkts >= 128
        || (g.nprobe_pkts >= 8 && g.nprobe_pkts.saturating_sub(g.nprobe_fail) < g.nprobe_pkts / 4)
}

// upstream: ieee80211_ra.c ieee80211_ra_add_stats_ht()
pub fn add_stats_ht(rn: &mut RaNode, peer: &HtPeer, mcs: u8, total: u32, fail: u32) {
    if usize::from(mcs) >= MCS_COUNT || total == 0 {
        return;
    }
    let g = &mut rn.g[mcs as usize];
    g.nprobe_pkts = g.nprobe_pkts.wrapping_add(total);
    g.nprobe_fail = g.nprobe_fail.wrapping_add(fail);
    if !probe_valid(g) {
        return;
    }
    rn.valid_probes |= bit(mcs);
    if g.nprobe_fail > g.nprobe_pkts {
        g.nprobe_fail = g.nprobe_pkts;
    }
    let sfer = (u64::from(g.nprobe_fail) << RA_FP_SHIFT) / u64::from(g.nprobe_pkts);
    g.nprobe_fail = 0;
    g.nprobe_pkts = 0;
    let rate = get_txrate(mcs, peer.supports_40mhz, use_ht_sgi(peer));
    g.loss = sfer * 100;
    g.measured = ((RA_FP_ONE - sfer) * rate) >> RA_FP_SHIFT;
    g.average = (((RA_FP_ONE - RA_FP_ONE / 8) * g.average) >> RA_FP_SHIFT)
        + ((RA_FP_ONE / 8 * g.measured) >> RA_FP_SHIFT);
    g.stddeviation = ((RA_FP_ONE - RA_FP_ONE / 4) * g.stddeviation) >> RA_FP_SHIFT;
    g.stddeviation += ((RA_FP_ONE / 4) * g.average.abs_diff(g.measured)) >> RA_FP_SHIFT;
}

// upstream: ieee80211_ra.c ieee80211_ra_choose()
pub fn choose(rn: &mut RaNode, peer: &mut HtPeer) {
    let g = rn.g[peer.tx_mcs as usize];
    if rn.valid_rates == 0 {
        rn.valid_rates = valid_rates(peer);
    }
    if rn.probing != 0 {
        if rn.valid_probes & bit(peer.tx_mcs) == 0 {
            return;
        }
        probe_clear(rn, peer);
        if !intra_mode_ra_finished(rn, peer) {
            probe_next_rate(rn, peer);
        } else if inter_mode_ra_finished(rn, peer) {
            rn.best_mcs = best_rate(rn);
            peer.tx_mcs = rn.best_mcs;
            probe_done(rn);
        }
        return;
    } else {
        rn.valid_probes = 0;
    }
    let rs = get_ht_rateset(peer.tx_mcs, peer.supports_40mhz, use_ht_sgi(peer));
    if (g.measured >> RA_FP_SHIFT) == 0
        || (g.average >= 3 * g.stddeviation && g.measured < g.average - 3 * g.stddeviation)
    {
        rn.probing = RA_PROBING_DOWN;
        rn.probed_rates = 0;
        if peer.tx_mcs == rs.min_mcs {
            if let Some(next) = next_rateset(rn, peer).copied() {
                probe_next_rateset(rn, peer, &next);
            } else {
                rn.probing = RA_NOT_PROBING;
            }
        } else {
            peer.tx_mcs = next_mcs(rn, peer);
            rn.candidate_rates = bit(peer.tx_mcs);
        }
    } else if g.loss < 2 * RA_FP_ONE || g.measured > g.average + 3 * g.stddeviation {
        rn.probing = RA_PROBING_UP;
        rn.probed_rates = 0;
        if peer.tx_mcs == rs.max_mcs {
            if let Some(next) = next_rateset(rn, peer).copied() {
                probe_next_rateset(rn, peer, &next);
            } else {
                rn.probing = RA_NOT_PROBING;
            }
        } else {
            peer.tx_mcs = next_mcs(rn, peer);
            rn.candidate_rates = bit(peer.tx_mcs);
        }
    } else {
        rn.probing = RA_NOT_PROBING;
        rn.probed_rates = 0;
        rn.candidate_rates = 0;
    }
}

// upstream: ieee80211_ra.c ieee80211_ra_node_init()
pub fn node_init(rn: &mut RaNode) {
    *rn = RaNode::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ratesets_sgi_and_fixed_point_match_openbsd_table() {
        assert_eq!(get_txrate(7, false, false), 130u64 * RA_FP_ONE / 2); // 65 Mbit/s in Q21
        let mut p = HtPeer {
            channel_40mhz: true,
            supports_40mhz: true,
            sgi40: true,
            ..Default::default()
        };
        assert!(use_ht_sgi(&p));
        p.sgi40 = false;
        assert!(!use_ht_sgi(&p));
        assert_eq!(
            fixedp_split((12 << RA_FP_SHIFT) + (RA_FP_ONE / 2)),
            (12, 50)
        );
    }
    #[test]
    fn selection_obeys_peer_masks_and_moves_between_ratesets() {
        let mut p = HtPeer {
            tx_mcs: 2,
            supports_40mhz: false,
            rx_mcs: u32::MAX,
            supported_tx_mcs: u32::MAX,
            ..Default::default()
        };
        let mut rn = RaNode {
            valid_rates: u32::MAX,
            probing: RA_PROBING_UP,
            ..Default::default()
        };
        assert_eq!(next_intra_rate(&rn, &p), 3);
        assert_eq!(next_lower_intra_rate(&rn, &p), 1);
        assert_eq!(next_rateset(&rn, &p).unwrap().min_mcs, 8);
        let next = *next_rateset(&rn, &p).unwrap();
        probe_next_rateset(&mut rn, &mut p, &next);
        assert!((8..=15).contains(&p.tx_mcs));
        assert_eq!(valid_rates(&p), u32::MAX);
    }
    #[test]
    fn probe_statistics_and_node_reset_follow_thresholds() {
        let peer = HtPeer {
            tx_mcs: 0,
            supports_40mhz: false,
            rx_mcs: u32::MAX,
            supported_tx_mcs: u32::MAX,
            ..Default::default()
        };
        let mut rn = RaNode::default();
        add_stats_ht(&mut rn, &peer, 0, 8, 7);
        assert_ne!(rn.valid_probes & 1, 0);
        assert_eq!(rn.g[0].nprobe_pkts, 0);
        node_init(&mut rn);
        assert_eq!(rn, RaNode::default());
        assert!(probe_valid(&GoodputStats {
            nprobe_pkts: 128,
            ..Default::default()
        }));
    }
}
