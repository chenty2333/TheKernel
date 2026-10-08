//! The 802.11 station protocol substrate.
//!
//! This crate translates the station-mode Ethernet/802.11 frame conversion
//! performed by OpenBSD net80211 and defines shared wireless frame types.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod channel;
mod frame;
mod node;
mod node_caps;
mod ra;
mod rates;
mod regdomain;
mod rsn;
mod rssadapt;

pub use channel::{
    CHAN_2GHZ, CHAN_5GHZ, ChannelRef, ModeSelection, NET_CAP_QOS, NET_CAP_TX_AMPDU, NET_CHAN_2GHZ,
    NET_CHAN_5GHZ, NET_CHAN_40MHZ, NET_CHAN_A, NET_CHAN_B, NET_CHAN_CCK, NET_CHAN_DYN, NET_CHAN_HT,
    NET_CHAN_OFDM, NET_CHAN_PASSIVE, NET_CHAN_PURE_G, NET_CHAN_VHT, NET_CHAN_X_80MHZ,
    NET_CHAN_X_160MHZ, NET_CHAN_X_HE, NET_FLAG_QOS, NetChannel, channel_ref_to_ieee,
    configure_ampdu_tx, find_rate, ieee_to_mhz, initialize_channels, mhz_to_ieee, next_scan_mode,
    select_mode,
};
pub use frame::{DecapError, EthernetFrame, MacAddress, decap_data, encap_station};
pub use node::{
    ASSOCFAIL_ESSID, ASSOCFAIL_PRIVACY, ASSOCFAIL_WPA_PROTO, AccessPoint, ESS_PSK, ESS_RSN_ON,
    ESS_WEP_ON, EssSelection, FLAG_AUTO_JOIN, HTOP0_SCO_MASK, HTOP0_SCO_SCA, HTOP0_SCO_SCB,
    HTOP0_SCO_SCN, HTOP0_SCO_SHIFT, NetworkProfile, PRIVACY, PROTO_RSN, PROTO_WPA, ess_adjust_rssi,
    ess_calculate_score, ess_is_better, get_ess, match_ess, switch_ess,
    valid_40mhz_center_frequency, valid_40mhz_secondary_above, valid_40mhz_secondary_below,
    valid_80mhz_center_frequency,
};
pub use node_caps::{
    HE_FIXED_CAPS_LEN, HE_MAC_CAPS_LEN, HE_MCS_NSS_80_LEN, HE_PHY_CAPS_LEN,
    HE_PHYCAP0_CHAN_WIDTH_160_IN_5G, HE_PHYCAP0_CHAN_WIDTH_8080_IN_5G, HeCapabilities,
    HtCapabilities, HtOperation, NODE_HE, NODE_HECAP, NODE_HT, NODE_HT_SGI20, NODE_HT_SGI40,
    NODE_HTCAP, NODE_VHT, NODE_VHT_SGI80, NODE_VHT_SGI160, NODE_VHTCAP, VHTOP0_CHAN_WIDTH_80,
    VHTOP0_CHAN_WIDTH_160, VHTOP0_CHAN_WIDTH_8080, VHTOP0_CHAN_WIDTH_HT, VhtCapabilities,
    clear_he_caps, clear_ht_caps, clear_vht_caps, ht_secondary_offset, setup_he_caps,
    setup_he_operation, setup_ht_caps, setup_ht_operation, setup_vht_caps, vht_channel_width,
};
pub use ra::{
    GoodputStats, HT_RATESETS, HtPeer, HtRateSet, MCS_COUNT, RA_FP_ONE, RA_FP_SHIFT,
    RA_NOT_PROBING, RA_PROBING_DOWN, RA_PROBING_INTER, RA_PROBING_UP, RaNode, add_stats_ht,
    best_mcs_in_rateset, best_rate, choose, fixedp_split, fixedp_string, get_ht_rateset,
    get_txrate, inter_mode_ra_finished, intra_mode_ra_finished, next_intra_rate,
    next_lower_intra_rate, next_mcs, next_rateset, node_init, probe_clear, probe_done,
    probe_next_rate, probe_next_rateset, probe_valid, trigger_next_rateset, use_ht_sgi,
    valid_rates, valid_tx_mcs,
};
pub use rates::{
    PhyMode, RATE_BASIC as LEGACY_RATE_BASIC, RATE_MAX_SIZE, RATE_VALUE as LEGACY_RATE_VALUE,
    RateSet, STANDARD_RATES_11A, STANDARD_RATES_11B, STANDARD_RATES_11G, max_basic_rate,
    min_basic_rate, plcp_to_rate, rate_to_plcp, set_basic_rates,
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
