//! VHT goodput rate adaptation from OpenBSD net80211.
//!
//! This is the station-side `ieee80211_ra_vht.c` algorithm and rateset
//! contract. iwx itself uses firmware TLC for rate selection; the associated
//! peer's VHT rate notification is kept as the selected host-visible state.
//! Copyright (c) 2021 Christian Ehrhardt; Copyright (c) 2016, 2021, 2022
//! Stefan Sperling; Copyright (c) 2016 Theo Buehler.
//! Permission to use, copy, modify, and distribute this software for any
//! purpose with or without fee is hereby granted, provided that the above
//! copyright notice and this permission notice appear in all copies.
//! THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES.

pub const VHT_NUM_SS: usize = 2;
pub const VHT_MAX_MCS: usize = 10;
pub const VHT_NUM_RATESETS: usize = 12;
pub const VHT_RATESET_MAX_NRATES: usize = 10;
pub const RA_FP_SHIFT: u32 = super::ra::RA_FP_SHIFT;
pub const RA_FP_ONE: u64 = 1 << RA_FP_SHIFT;
pub const RA_RATE_THRESHOLD: u64 = RA_FP_ONE / 64;
pub const RA_NOT_PROBING: u32 = 0;
pub const RA_PROBING_DOWN: u32 = 1;
pub const RA_PROBING_UP: u32 = 2;
pub const RA_PROBING_INTER: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VhtRateSet {
    pub index: usize,
    pub rates: [u16; VHT_RATESET_MAX_NRATES],
    pub num_rates: usize,
    pub num_ss: u8,
    pub chan40: bool,
    pub chan80: bool,
    pub sgi: bool,
}

const fn rs(
    index: usize,
    rates: [u16; 10],
    num_rates: usize,
    nss: u8,
    c40: bool,
    c80: bool,
    sgi: bool,
) -> VhtRateSet {
    VhtRateSet {
        index,
        rates,
        num_rates,
        num_ss: nss,
        chan40: c40,
        chan80: c80,
        sgi,
    }
}

/// OpenBSD `ieee80211_std_ratesets_11ac[]`, in 500-kbit/s units.
pub const VHT_RATESETS: [VhtRateSet; VHT_NUM_RATESETS] = [
    rs(
        0,
        [13, 26, 39, 52, 78, 104, 117, 130, 156, 0],
        9,
        1,
        false,
        false,
        false,
    ),
    rs(
        1,
        [14, 29, 43, 58, 87, 116, 130, 144, 174, 0],
        9,
        1,
        false,
        false,
        true,
    ),
    rs(
        2,
        [26, 52, 78, 104, 156, 208, 234, 260, 312, 0],
        9,
        2,
        false,
        false,
        false,
    ),
    rs(
        3,
        [29, 58, 87, 116, 173, 231, 261, 289, 347, 0],
        9,
        2,
        false,
        false,
        true,
    ),
    rs(
        4,
        [27, 54, 81, 108, 162, 216, 243, 270, 324, 360],
        10,
        1,
        true,
        false,
        false,
    ),
    rs(
        5,
        [30, 60, 90, 120, 180, 240, 270, 300, 360, 400],
        10,
        1,
        true,
        false,
        true,
    ),
    rs(
        6,
        [54, 108, 162, 216, 324, 432, 486, 540, 648, 720],
        10,
        2,
        true,
        false,
        false,
    ),
    rs(
        7,
        [60, 120, 180, 240, 360, 480, 540, 600, 720, 800],
        10,
        2,
        true,
        false,
        true,
    ),
    rs(
        8,
        [59, 117, 176, 234, 351, 468, 527, 585, 702, 780],
        10,
        1,
        false,
        true,
        false,
    ),
    rs(
        9,
        [65, 130, 195, 260, 390, 520, 585, 650, 780, 867],
        10,
        1,
        false,
        true,
        true,
    ),
    rs(
        10,
        [117, 234, 351, 468, 702, 936, 1053, 1404, 1560, 0],
        9,
        2,
        false,
        true,
        false,
    ),
    rs(
        11,
        [130, 260, 390, 520, 780, 1040, 1170, 1300, 1560, 1734],
        10,
        2,
        false,
        true,
        true,
    ),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VhtRaNode {
    pub valid_probes: [u16; VHT_NUM_SS],
    pub valid_rates: [u16; VHT_NUM_SS],
    pub candidate_rates: [u16; VHT_NUM_SS],
    pub probed_rates: [u16; VHT_NUM_SS],
    pub max_mcs: [u8; VHT_NUM_SS],
    pub probing: u32,
    pub best_mcs: u8,
    pub best_nss: u8,
    pub g: [[GoodputStats; VHT_RATESET_MAX_NRATES]; VHT_NUM_RATESETS],
}
impl Default for VhtRaNode {
    fn default() -> Self {
        Self {
            valid_probes: [0; 2],
            valid_rates: [0; 2],
            candidate_rates: [0; 2],
            probed_rates: [0; 2],
            max_mcs: [0; 2],
            probing: 0,
            best_mcs: 0,
            best_nss: 1,
            g: [[GoodputStats::default(); 10]; 12],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VhtPeer {
    pub tx_mcs: u8,
    pub tx_nss: u8,
    pub rx_mcs_map: u16,
    pub local_tx_mcs_map: u16,
    pub channel_40: bool,
    pub channel_80: bool,
    pub channel_160: bool,
    pub peer_40: bool,
    pub peer_80: bool,
    pub peer_160: bool,
    pub sgi80: bool,
    pub sgi160: bool,
}

// upstream: ieee80211_ra_vht_get_rateset()
pub fn get_rateset(mcs: u8, nss: u8, chan40: bool, chan80: bool, sgi: bool) -> &'static VhtRateSet {
    VHT_RATESETS
        .iter()
        .find(|r| {
            usize::from(mcs) < r.num_rates
                && r.num_ss == nss
                && r.chan40 == chan40
                && r.chan80 == chan80
                && r.sgi == sgi
        })
        .expect("VHT MCS/NSS has no rateset")
}
// upstream: ieee80211_ra_vht_use_sgi()
pub fn use_sgi(p: &VhtPeer) -> bool {
    (p.channel_160 && p.peer_160 && p.sgi160) || (p.channel_80 && p.peer_80 && p.sgi80)
}
// upstream: ieee80211_ra_vht_get_txrate()
pub fn get_txrate(mcs: u8, nss: u8, chan40: bool, chan80: bool, sgi: bool) -> u64 {
    (u64::from(get_rateset(mcs, nss, chan40, chan80, sgi).rates[mcs as usize]) << RA_FP_SHIFT) * 500
        / 1000
}
// upstream: ieee80211_ra_vht_next_lower_intra_rate()
pub fn next_lower_intra_rate(_rn: &VhtRaNode, p: &VhtPeer) -> u8 {
    p.tx_mcs.saturating_sub(1)
}
// upstream: ieee80211_ra_vht_get_max_mcs()
pub fn get_max_mcs(map: u16, nss: u8, chan40: bool) -> Option<u8> {
    if !(1..=8).contains(&nss) {
        return None;
    }
    let supp = (map >> (2 * (nss - 1))) & 3;
    match supp {
        0 => Some(7),
        1 => Some(8),
        2 => Some(if chan40 { 9 } else { 8 }),
        3 => None,
        _ => None,
    }
}
// upstream: ieee80211_ra_vht_next_intra_rate()
pub fn next_intra_rate(rn: &VhtRaNode, p: &VhtPeer) -> u8 {
    let max = rn.max_mcs[usize::from(p.tx_nss.saturating_sub(1))];
    if p.tx_mcs >= max { max } else { p.tx_mcs + 1 }
}

fn current_80_set(p: &VhtPeer) -> &'static VhtRateSet {
    get_rateset(p.tx_mcs, p.tx_nss, false, true, use_sgi(p))
}
// upstream: ieee80211_ra_vht_next_rateset()
pub fn next_rateset(rn: &VhtRaNode, p: &VhtPeer) -> Option<&'static VhtRateSet> {
    let current = current_80_set(p);
    let next = match (
        current.index,
        rn.probing & RA_PROBING_UP != 0,
        rn.probing & RA_PROBING_DOWN != 0,
    ) {
        (8, true, _) => 10,
        (9, true, _) => 11,
        (10, true, _) => return None,
        (11, true, _) => return None,
        (10, false, true) => 8,
        (11, false, true) => 9,
        (8, false, true) => return None,
        (9, false, true) => return None,
        _ => return None,
    };
    let rs = &VHT_RATESETS[next];
    (rn.valid_rates[usize::from(rs.num_ss - 1)] != 0).then_some(rs)
}
// upstream: ieee80211_ra_vht_best_mcs_in_rateset()
pub fn best_mcs_in_rateset(rn: &VhtRaNode, rs: &VhtRateSet) -> u8 {
    let mask = rn.valid_rates[usize::from(rs.num_ss - 1)];
    let mut best = 0;
    let mut max = 0;
    for m in 0..rs.num_rates {
        let g = rn.g[rs.index][m].measured;
        if mask & (1 << m) != 0 && g > max + RA_RATE_THRESHOLD {
            max = g;
            best = m as u8
        }
    }
    best
}
// upstream: ieee80211_ra_vht_probe_next_rateset()
pub fn probe_next_rateset(rn: &mut VhtRaNode, p: &mut VhtPeer, next: &VhtRateSet) {
    let current = current_80_set(p);
    let best = best_mcs_in_rateset(rn, current);
    let measured = rn.g[current.index][usize::from(best)].measured;
    p.tx_mcs = 0;
    p.tx_nss = next.num_ss;
    let mask = rn.valid_rates[usize::from(next.num_ss - 1)];
    let mut selected = false;
    for m in 0..next.num_rates {
        if mask & (1 << m) == 0 {
            continue;
        }
        let rate = (u64::from(next.rates[m]) * 500 << RA_FP_SHIFT) / 1000;
        if rate > measured + RA_RATE_THRESHOLD {
            p.tx_mcs = m as u8;
            selected = true;
            break;
        }
    }
    if !selected {
        p.tx_mcs = best_mcs_in_rateset(rn, next);
    }
    let i = usize::from(next.num_ss - 1);
    rn.candidate_rates[i] |= 1 << p.tx_mcs;
    let candidate = if rn.probing & RA_PROBING_UP != 0 {
        next_intra_rate(rn, p)
    } else {
        next_lower_intra_rate(rn, p)
    };
    rn.candidate_rates[i] |= 1 << candidate;
}
// upstream: ieee80211_ra_vht_next_mcs()
pub fn next_mcs(rn: &VhtRaNode, p: &VhtPeer) -> u8 {
    if rn.probing & RA_PROBING_DOWN != 0 {
        next_lower_intra_rate(rn, p)
    } else {
        next_intra_rate(rn, p)
    }
}
// upstream: ieee80211_ra_vht_probe_clear()
pub fn probe_clear(g: &mut GoodputStats) {
    g.nprobe_pkts = 0;
    g.nprobe_fail = 0
}
// upstream: ieee80211_ra_vht_probe_done()
pub fn probe_done(rn: &mut VhtRaNode, nss: u8) {
    let i = usize::from(nss.saturating_sub(1));
    if i < VHT_NUM_SS {
        rn.probing = RA_NOT_PROBING;
        rn.probed_rates[i] = 0;
        rn.valid_probes[i] = 0;
        rn.candidate_rates[i] = 0
    }
}
// upstream: ieee80211_ra_vht_intra_mode_ra_finished()
pub fn intra_mode_ra_finished(rn: &mut VhtRaNode, p: &mut VhtPeer) -> bool {
    let i = usize::from(p.tx_nss.saturating_sub(1));
    rn.probed_rates[i] |= 1 << p.tx_mcs;
    let rs = current_80_set(p);
    if rn.probing & RA_PROBING_DOWN != 0 && (p.tx_mcs == 0 || rn.probed_rates[i] & 1 != 0) {
        trigger_next_rateset(rn, p);
        return true;
    }
    if rn.probing & RA_PROBING_UP != 0
        && (p.tx_mcs == rn.max_mcs[i] || rn.probed_rates[i] & (1 << rn.max_mcs[i]) != 0)
    {
        trigger_next_rateset(rn, p);
        return true;
    }
    let next = next_mcs(rn, p);
    if next == p.tx_mcs || {
        let g = rn.g[rs.index][usize::from(p.tx_mcs)];
        g.loss == 0
            && g.measured >= get_txrate(next, p.tx_nss, false, true, use_sgi(p)) + RA_RATE_THRESHOLD
    } {
        trigger_next_rateset(rn, p);
        return true;
    }
    let best = best_mcs_in_rateset(rn, rs);
    if (rn.probing & RA_PROBING_UP != 0 && best < p.tx_mcs)
        || (rn.probing & RA_PROBING_DOWN != 0 && best > p.tx_mcs)
    {
        trigger_next_rateset(rn, p);
        return true;
    }
    rn.candidate_rates[i] & rn.probed_rates[i] == rn.candidate_rates[i]
}
// upstream: ieee80211_ra_vht_trigger_next_rateset()
pub fn trigger_next_rateset(rn: &mut VhtRaNode, p: &mut VhtPeer) {
    if let Some(next) = next_rateset(rn, p).copied() {
        probe_next_rateset(rn, p, &next);
        rn.probing |= RA_PROBING_INTER
    } else {
        rn.probing &= !RA_PROBING_INTER
    }
}
// upstream: ieee80211_ra_vht_inter_mode_ra_finished()
pub fn inter_mode_ra_finished(rn: &VhtRaNode) -> bool {
    rn.probing & RA_PROBING_INTER == 0
}
// upstream: ieee80211_ra_vht_best_rate()
pub fn best_rate(rn: &mut VhtRaNode, p: &VhtPeer) {
    let mut bm = rn.best_mcs;
    let mut bn = rn.best_nss;
    let first = get_rateset(bm, bn, false, true, use_sgi(p));
    let mut max = rn.g[first.index][usize::from(bm)].measured;
    for rs in VHT_RATESETS.iter() {
        for m in 0..rs.num_rates {
            if rn.valid_rates[usize::from(rs.num_ss - 1)] & (1 << m) == 0 {
                continue;
            }
            let good = rn.g[rs.index][m].measured;
            if good > max + RA_RATE_THRESHOLD {
                max = good;
                bm = m as u8;
                bn = rs.num_ss
            }
        }
    }
    rn.best_mcs = bm;
    rn.best_nss = bn
}
// upstream: ieee80211_ra_vht_probe_next_rate()
pub fn probe_next_rate(rn: &mut VhtRaNode, p: &mut VhtPeer) {
    let i = usize::from(p.tx_nss - 1);
    rn.probed_rates[i] |= 1 << p.tx_mcs;
    p.tx_mcs = next_mcs(rn, p)
}
// upstream: ieee80211_ra_vht_init_valid_rates()
pub fn init_valid_rates(rn: &mut VhtRaNode, local_map: u16, peer_map: u16, chan40: bool) {
    rn.max_mcs = [0; 2];
    rn.valid_rates = [0; 2];
    for nss in 1..=2 {
        if let (Some(a), Some(b)) = (
            get_max_mcs(local_map, nss, chan40),
            get_max_mcs(peer_map, nss, chan40),
        ) {
            let max = a.min(b);
            rn.max_mcs[usize::from(nss - 1)] = max;
            rn.valid_rates[usize::from(nss - 1)] = (1 << (max + 1)) - 1
        }
    }
}
// upstream: ieee80211_ra_vht_probe_valid()
pub fn probe_valid(g: &GoodputStats) -> bool {
    g.nprobe_pkts >= 128
        || (g.nprobe_pkts >= 8 && g.nprobe_pkts.saturating_sub(g.nprobe_fail) < g.nprobe_pkts / 4)
}
// upstream: ieee80211_ra_vht_add_stats()
pub fn add_stats(rn: &mut VhtRaNode, p: &VhtPeer, mcs: u8, nss: u8, total: u32, fail: u32) {
    if usize::from(mcs) >= 10 || !(1..=2).contains(&nss) || total == 0 {
        return;
    }
    let rs = get_rateset(mcs, nss, false, true, use_sgi(p));
    let g = &mut rn.g[rs.index][usize::from(mcs)];
    g.nprobe_pkts = g.nprobe_pkts.wrapping_add(total);
    g.nprobe_fail = g.nprobe_fail.wrapping_add(fail);
    if !probe_valid(g) {
        return;
    }
    rn.valid_probes[usize::from(nss - 1)] |= 1 << mcs;
    if g.nprobe_fail > g.nprobe_pkts {
        g.nprobe_fail = g.nprobe_pkts
    }
    let sfer = (u64::from(g.nprobe_fail) << RA_FP_SHIFT) / u64::from(g.nprobe_pkts);
    probe_clear(g);
    let rate = get_txrate(mcs, nss, false, true, use_sgi(p));
    g.loss = sfer * 100;
    g.measured = ((RA_FP_ONE - sfer) * rate) >> RA_FP_SHIFT;
    g.average = (((RA_FP_ONE - RA_FP_ONE / 8) * g.average) >> RA_FP_SHIFT)
        + ((RA_FP_ONE / 8 * g.measured) >> RA_FP_SHIFT);
    g.stddeviation = (((RA_FP_ONE - RA_FP_ONE / 4) * g.stddeviation) >> RA_FP_SHIFT)
        + ((RA_FP_ONE / 4 * g.average.abs_diff(g.measured)) >> RA_FP_SHIFT)
}
// upstream: ieee80211_ra_vht_choose()
pub fn choose(rn: &mut VhtRaNode, p: &mut VhtPeer) {
    if rn.valid_rates[0] == 0 {
        init_valid_rates(
            rn,
            p.local_tx_mcs_map,
            p.rx_mcs_map,
            p.channel_40 && p.peer_40,
        );
        assert_ne!(rn.valid_rates[0], 0, "VHT not supported")
    }
    let i = usize::from(p.tx_nss.saturating_sub(1));
    let rs = current_80_set(p);
    let g = rn.g[rs.index][usize::from(p.tx_mcs)];
    if rn.probing != 0 {
        if rn.valid_probes[i] & (1 << p.tx_mcs) == 0 {
            return;
        }
        probe_clear(&mut rn.g[rs.index][usize::from(p.tx_mcs)]);
        if !intra_mode_ra_finished(rn, p) {
            probe_next_rate(rn, p)
        } else if inter_mode_ra_finished(rn) {
            best_rate(rn, p);
            p.tx_mcs = rn.best_mcs;
            p.tx_nss = rn.best_nss;
            probe_done(rn, i as u8 + 1)
        }
        return;
    }
    rn.valid_probes[i] = 0;
    if g.measured >> RA_FP_SHIFT == 0
        || (g.average >= 3 * g.stddeviation && g.measured < g.average - 3 * g.stddeviation)
    {
        rn.probing = RA_PROBING_DOWN;
        rn.probed_rates[i] = 0;
        if p.tx_mcs == 0 {
            if let Some(next) = next_rateset(rn, p).copied() {
                probe_next_rateset(rn, p, &next)
            } else {
                rn.probing = 0
            }
        } else {
            p.tx_mcs = next_mcs(rn, p);
            rn.candidate_rates[i] = 1 << p.tx_mcs
        }
    } else if g.loss < 2 * RA_FP_ONE || g.measured > g.average + 3 * g.stddeviation {
        rn.probing = RA_PROBING_UP;
        rn.probed_rates[i] = 0;
        if p.tx_mcs == rn.max_mcs[i] {
            if let Some(next) = next_rateset(rn, p).copied() {
                probe_next_rateset(rn, p, &next)
            } else {
                rn.probing = 0
            }
        } else {
            p.tx_mcs = next_mcs(rn, p);
            rn.candidate_rates[i] = 1 << p.tx_mcs
        }
    } else {
        rn.probing = 0;
        rn.probed_rates[i] = 0;
        rn.candidate_rates[i] = 0
    }
}
// upstream: ieee80211_ra_vht_node_init()
pub fn node_init(rn: &mut VhtRaNode) {
    *rn = VhtRaNode::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn openbsd_ratesets_width_sgi_and_max_mcs() {
        assert_eq!(get_txrate(9, 1, false, true, false), 780 * RA_FP_ONE / 2);
        assert_eq!(get_rateset(8, 2, false, true, true).index, 11);
        assert_eq!(get_max_mcs(2, 1, false), Some(8));
        assert_eq!(get_max_mcs(2, 1, true), Some(9));
        assert_eq!(get_max_mcs(3, 1, true), None);
        let p = VhtPeer {
            channel_80: true,
            peer_80: true,
            sgi80: true,
            ..Default::default()
        };
        assert!(use_sgi(&p));
    }
    #[test]
    fn two_stream_probe_walks_rateset_and_ignores_unsupported_peer() {
        let mut rn = VhtRaNode::default();
        init_valid_rates(&mut rn, 2 | (2 << 2), 2 | (2 << 2), true);
        assert_eq!(rn.valid_rates, [0x3ff, 0x3ff]);
        let mut p = VhtPeer {
            tx_nss: 1,
            tx_mcs: 8,
            rx_mcs_map: 2 | (2 << 2),
            local_tx_mcs_map: 2 | (2 << 2),
            peer_40: true,
            ..Default::default()
        };
        rn.probing = RA_PROBING_UP;
        let next = *next_rateset(&rn, &p).unwrap();
        assert_eq!(next.index, 10);
        probe_next_rateset(&mut rn, &mut p, &next);
        assert_eq!(p.tx_nss, 2);
        assert_eq!(p.tx_mcs, 0);
        assert_ne!(rn.candidate_rates[1] & 1, 0);
    }
    #[test]
    fn vht_measurement_saturates_failures_and_resets_probe() {
        let mut rn = VhtRaNode::default();
        let p = VhtPeer {
            tx_nss: 1,
            peer_40: true,
            ..Default::default()
        };
        add_stats(&mut rn, &p, 0, 1, 8, 9);
        let rs = get_rateset(0, 1, false, true, false);
        assert_eq!(rn.g[rs.index][0].loss, RA_FP_ONE * 100);
        assert_eq!(rn.g[rs.index][0].nprobe_pkts, 0);
        assert!(probe_valid(&GoodputStats {
            nprobe_pkts: 128,
            ..Default::default()
        }));
        node_init(&mut rn);
        assert_eq!(rn.best_nss, 1);
    }
}
