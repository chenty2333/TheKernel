//! The 802.11 station protocol substrate.
//!
//! This crate translates the station-mode Ethernet/802.11 frame conversion
//! performed by OpenBSD net80211 and defines shared wireless frame types.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod channel;
mod frame;
mod ra;
mod regdomain;
mod rsn;
mod rssadapt;

pub use channel::{
    CHAN_2GHZ, CHAN_5GHZ, ChannelRef, channel_ref_to_ieee, ieee_to_mhz, mhz_to_ieee,
};
pub use frame::{DecapError, EthernetFrame, MacAddress, decap_data, encap_station};
pub use ra::{
    GoodputStats, HT_RATESETS, HtPeer, HtRateSet, MCS_COUNT, RA_FP_ONE, RA_FP_SHIFT,
    RA_NOT_PROBING, RA_PROBING_DOWN, RA_PROBING_INTER, RA_PROBING_UP, RaNode, add_stats_ht,
    best_mcs_in_rateset, best_rate, choose, fixedp_split, fixedp_string, get_ht_rateset,
    get_txrate, inter_mode_ra_finished, intra_mode_ra_finished, next_intra_rate,
    next_lower_intra_rate, next_mcs, next_rateset, node_init, probe_clear, probe_done,
    probe_next_rate, probe_next_rateset, probe_valid, trigger_next_rateset, use_ht_sgi,
    valid_rates, valid_tx_mcs,
};
pub use regdomain::{
    CHANNELS_5GHZ_MAX, CHANNELS_5GHZ_MIN, COUNTRY_NAMES, CountryName, DMN_DEBUG, DMN_DEFAULT,
    REGDOMAIN_MAP, REGDOMAIN_NAMES, RegdomainMap, RegdomainName, compare_country_name,
    compare_regdomain_name, country_code_to_name, country_code_to_regdomain, name_to_country_code,
    name_to_regdomain, regdomain_to_flag, regdomain_to_name,
};
pub use rsn::{Akm, Cipher, RsnParams, RsnStatus, parse_akm, parse_cipher, parse_rsn, parse_wpa};
pub use rssadapt::{
    LegacyRateSet, RATE_BASIC, RATE_SIZE, RATE_VALUE, RSSADAPT_BUCKET_POWER, RSSADAPT_BUCKET0,
    RSSADAPT_BUCKETS, RssAdapt, RssDescriptor, choose_rate, input_rssi, lower_rate, raise_rate,
    update_stats,
};
