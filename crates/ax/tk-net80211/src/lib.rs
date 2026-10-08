//! The 802.11 station protocol substrate.
//!
//! This crate translates the station-mode Ethernet/802.11 frame conversion
//! performed by OpenBSD net80211 and defines shared wireless frame types.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod assoc_rx;
mod auth_rx;
mod ba_rx;
mod ba_tx;
mod beacon;
mod channel;
mod crypto;
mod crypto_bip;
mod crypto_ccmp;
mod crypto_tkip;
mod crypto_wep;
mod decrypt;
mod disconnect_rx;
mod frame;
mod input;
mod mgmt_rx;
mod media;
mod node;
mod node_caps;
mod node_rates;
mod node_table;
mod output;
mod proto;
mod ra;
mod rates;
mod regdomain;
mod rsn;
mod rssadapt;
mod rx_path;
mod sa_query;
mod scan;

pub use assoc_rx::{AssocRxError, AssocRxPolicy, AssocRxResult, receive_assoc_response};
pub use auth_rx::{AuthRxError, AuthRxResult, receive_auth_response};
pub use ba_rx::{
    ActionDispatch, AddbaRequestOutcome, AddbaRequestPolicy, AddbaResponseOutcome, BaAgreement,
    BaRxError, BarTidOutcome, DelbaOutcome, accept_addba_request, accept_addba_response,
    apply_bar_tid, dispatch_action, receive_addba_request, receive_addba_response, receive_bar,
    receive_delba, refuse_addba_request, refuse_addba_response,
};
pub use ba_tx::{
    ADD_BA_AMSDU, ADD_BA_MAX_WINDOW, ADD_BA_POLICY, ADD_BA_REQUEST_INTERVAL_MAX,
    ADD_BA_RESPONSE_TIMEOUT_MICROS, ADD_BA_STATUS_UNSPECIFIED, ADD_BA_TID_SHIFT,
    ADD_BA_WINDOW_SHIFT, AddbaTxOutcome, AddbaTxPolicy, BA_TID_COUNT, DELBA_REASON_AUTH_LEAVE,
    DELBA_REASON_SETUP_REQUIRED, DELBA_REASON_TIMEOUT, DelbaRequestEffects, RxBaTimeoutEffects,
    StopAmpduTidEffect, TX_BA_AGREED, TX_BA_INIT, TX_BA_REQUESTED, TxBaAgreement,
    TxBaTimeoutEffects, request_delba, rx_ba_timeout, start_addba_request, stop_ampdu_tx,
    tx_ba_timeout,
};
pub use beacon::{BeaconError, BeaconPolicy, BeaconRxInfo, BeaconUpdate, receive_beacon};
pub use channel::{
    CHAN_2GHZ, CHAN_5GHZ, ChannelRef, ModeSelection, NET_CAP_QOS, NET_CAP_TX_AMPDU, NET_CHAN_2GHZ,
    NET_CHAN_5GHZ, NET_CHAN_40MHZ, NET_CHAN_A, NET_CHAN_B, NET_CHAN_CCK, NET_CHAN_DYN, NET_CHAN_HT,
    NET_CHAN_OFDM, NET_CHAN_PASSIVE, NET_CHAN_PURE_G, NET_CHAN_VHT, NET_CHAN_X_80MHZ,
    NET_CHAN_X_160MHZ, NET_CHAN_X_HE, NET_FLAG_QOS, NetChannel, channel_ref_to_ieee,
    configure_ampdu_tx, find_rate, ieee_to_mhz, initialize_channels, mhz_to_ieee, next_scan_mode,
    select_mode,
};
pub use crypto::{
    KeySelection, SoftwareCryptoError, SoftwareKey, cipher_key_length, decrypt_software,
    delete_software_key, encrypt_software, select_rx_key, select_tx_key, set_software_key,
};
pub use crypto_bip::{BipError, BipKey, bip_decap, bip_encap};
pub use crypto_ccmp::{CcmpError, CcmpKey, decrypt_ccmp, encrypt_ccmp};
pub use crypto_tkip::{
    MichaelFailureEffects, MichaelFailureState, TkipError, TkipKey, decrypt_tkip, encrypt_tkip,
    michael_mic_failure, tkip_mic,
};
pub use crypto_wep::{WepError, WepKey, decrypt_wep, encrypt_wep};
pub use decrypt::{
    HardwareDecryptError, HardwareDecryptResult, HardwareReplayState,
    postprocess_hardware_decryption,
};
pub use disconnect_rx::{
    DisconnectKind, DisconnectPolicy, DisconnectRxError, DisconnectRxResult, RxOperatingMode,
    receive_deauthentication, receive_disassociation,
};
pub use frame::{DecapError, EthernetFrame, MacAddress, decap_amsdu, decap_data, encap_station};
pub use input::{
    EdcaAcParams, EdcaError, EdcaState, EdcaUpdate, HeaderError, SaveIeError, UapsdState,
    has_address4, has_ht_control, has_qos_control, has_sequence_control, header_length,
    parse_edca_body, parse_edca_ie, parse_wmm_params, parse_wmm_qos_info, qos_control,
    save_information_element, setup_uapsd,
};
pub use mgmt_rx::{ManagementRxError, ManagementRxKind, receive_management_kind};
pub use media::{
    MediaConversionError, MediaSubtype, media_to_mcs, media_to_rate, mcs_to_media,
    rate_to_media,
};
pub use node::{
    AKM_8021X, AKM_PSK, AKM_SHA256_8021X, AKM_SHA256_PSK, ASSOCFAIL_BASIC_RATE, ASSOCFAIL_BSSID,
    ASSOCFAIL_CHAN, ASSOCFAIL_CSA, ASSOCFAIL_ESSID, ASSOCFAIL_IBSS, ASSOCFAIL_PRIVACY,
    ASSOCFAIL_WPA_KEY, ASSOCFAIL_WPA_PROTO, AccessPoint, BssMatchPolicy, BssSelection, CAPINFO_ESS,
    CAPINFO_IBSS, CIPHER_CCMP, CIPHER_TKIP, ESS_PSK, ESS_RSN_ON, ESS_WEP_ON, ErpLeaveEffects,
    EssSelection, FLAG_AUTO_JOIN, HTOP0_SCO_MASK, HTOP0_SCO_SCA, HTOP0_SCO_SCB, HTOP0_SCO_SCN,
    HTOP0_SCO_SHIFT, HtLeaveEffects, LOCAL_CAP_MFP, NetworkProfile, PRIVACY, PROTO_RSN, PROTO_WPA,
    RSN_CAP_MFPC, RsnLeaveEffects, StationBssJoinError, StationBssJoinPlan, StationBssJoinPolicy,
    check_rssi, choose_bss, choose_rsn_params, ess_adjust_rssi, ess_calculate_score, ess_is_better,
    get_ess, get_rate, get_rssi, join_station_bss, leave_11g_network, leave_he_network,
    leave_ht_network, leave_rsn_network, leave_vht_network, match_bss, match_ess, switch_ess,
    valid_40mhz_center_frequency, valid_40mhz_secondary_above, valid_40mhz_secondary_below,
    valid_80mhz_center_frequency,
};
pub use node_caps::{
    HE_FIXED_CAPS_LEN, HE_MAC_CAPS_LEN, HE_MCS_NSS_80_LEN, HE_MCS_SS_NOT_SUPP, HE_PHY_CAPS_LEN,
    HE_PHYCAP0_CHAN_WIDTH_160_IN_5G, HE_PHYCAP0_CHAN_WIDTH_8080_IN_5G, HTCAP_CBW20_40, HTCAP_SGI20,
    HTCAP_SGI40, HTOP0_CHW, HeCapabilities, HtCapabilities, HtOperation, NODE_HE, NODE_HECAP,
    NODE_HT, NODE_HT_SGI20, NODE_HT_SGI40, NODE_HTCAP, NODE_VHT, NODE_VHT_SGI80, NODE_VHT_SGI160,
    NODE_VHTCAP, VHT_MCS_SS_NOT_SUPP, VHTOP0_CHAN_WIDTH_80, VHTOP0_CHAN_WIDTH_160,
    VHTOP0_CHAN_WIDTH_8080, VHTOP0_CHAN_WIDTH_HT, VhtCapabilities, VhtOperation, clear_he_caps,
    clear_ht_caps, clear_vht_caps, ht_secondary_offset, setup_he_caps, setup_he_operation,
    setup_ht_caps, setup_ht_operation, setup_vht_caps, setup_vht_operation, supports_he,
    supports_ht, supports_ht_chan40, supports_ht_sgi20, supports_ht_sgi40, supports_vht,
    supports_vht_chan80, supports_vht_chan160, supports_vht_sgi80, supports_vht_sgi160,
    vht_channel_width,
};
pub use node_rates::{
    CHAN_DYN as NODE_CHAN_DYN, CHAN_OFDM as NODE_CHAN_OFDM, NODE_ERP, PeerRateState, RateIeError,
    node_abg_mode, node_is_11g, setup_rates,
};
pub use node_table::{
    INACT_SCAN, INVALID_SEQUENCE, NODE_CACHE_SIZE, NodeAllocError, NodeCopyEffects, NodeLifecycle,
    NodeRecord, NodeTable, alloc_node, allocation_available, clean_inactive_nodes, copy_node_state,
    duplicate_bss_node, find_node, find_node_mut, find_station_rx_node, find_station_tx_node,
    free_all_nodes, free_node,
    needs_station_rx_node, raise_scan_node_inactivity, setup_node,
};
pub use output::{
    ACTION_ADDBA_REQUEST, ACTION_ADDBA_RESPONSE, ACTION_CATEGORY_BLOCK_ACK,
    ACTION_CATEGORY_SA_QUERY, ACTION_DELBA, ACTION_SA_QUERY_REQUEST, ACTION_SA_QUERY_RESPONSE,
    AKM_8021X as IE_AKM_8021X, AKM_PSK as IE_AKM_PSK, AKM_SHA256_8021X as IE_AKM_SHA256_8021X,
    AKM_SHA256_PSK as IE_AKM_SHA256_PSK, AUTH_ALG_OPEN, ActionBodyFields, AmpduPolicy,
    CAPINFO_ESS as OUTPUT_CAPINFO_ESS, CAPINFO_IBSS as OUTPUT_CAPINFO_IBSS,
    CAPINFO_PRIVACY as OUTPUT_CAPINFO_PRIVACY, CAPINFO_SHORT_PREAMBLE, CAPINFO_SHORT_SLOTTIME,
    CIPHER_BIP as IE_CIPHER_BIP, CIPHER_CCMP as IE_CIPHER_CCMP, CIPHER_TKIP as IE_CIPHER_TKIP,
    CIPHER_USE_GROUP, CIPHER_WEP40, CIPHER_WEP104, ELEMID_DS_PARAMS, ELEMID_EDCA_PARAMS,
    ELEMID_ERP, ELEMID_EXT_HE_CAPS, ELEMID_EXTENSION, ELEMID_HT_CAPS, ELEMID_HT_OPERATION,
    ELEMID_QOS_CAPABILITY, ELEMID_RATES, ELEMID_RSN, ELEMID_SSID, ELEMID_VENDOR, ELEMID_VHT_CAPS,
    ELEMID_XRATES, ERP_BARKER_MODE, ERP_NON_ERP_PRESENT, ERP_USE_PROTECTION, EdcaTxopLimiter,
    IeError, ManagementFrameError, ManagementTxSequence, OutputOpMode, ProbeRequestConfig, RSN_OUI,
    RSNCAP_GTKSA_RCNT_MASK, RSNCAP_MFPC,
    RSNCAP_MFPR, RSNCAP_PBAC, RSNCAP_PTKSA_RCNT_MASK, RsnIePolicy, TxBaWindow, WPA_OUI,
    append_capability_info, append_ds_params_ie, append_edca_params_ie, append_erp_ie,
    append_extended_rates_ie, append_he_caps_ie, append_ht_caps_ie, append_ht_operation_ie,
    append_qos_capability_ie, append_rsn_ie, append_ssid_ie, append_supported_rates_ie,
    append_vht_caps_ie, append_wme_info_ie, append_wme_parameter_ie, append_wpa_ie,
    build_action_body, build_addba_request_body, build_addba_response_body, build_compressed_bar,
    build_assoc_request_body, build_auth_body, build_deauth_body, build_delba_body,
    build_disassoc_body, build_probe_request_ies, build_rsn_body, build_sa_query_body,
    can_use_ampdu, classify_ethernet_frame, move_tx_ba_window, uapsd_qos_info,
    user_priority_to_access_category,
};
pub use proto::{
    CAP_SHORT_PREAMBLE, CAP_SHORT_SLOT, ErpState, FIX_RATE_DELETE, FIX_RATE_FIXED,
    FIX_RATE_NEGOTIATE, FIX_RATE_SORT, FLAG_SHORT_PREAMBLE, FLAG_SHORT_SLOT, FLAG_USE_PROTECTION,
    FixRateConfig, KeyRunError, LocalPhyConfig, ManagementWatchdogEffects, NegotiatedPhy,
    OpenAuthEffects, OpenAuthState, PeerPhyConfig, ProtocolState, RsnSupplicantState,
    auth_open_station, beacon_miss_threshold, fix_rate, he_negotiate, ht_negotiate,
    management_watchdog_tick, mark_supplicant_key_failure, newstate, reset_erp, set_short_slot,
    start_station_keyrun, try_another_bss, vht_negotiate,
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
pub use rx_path::{RxDataPolicy, RxDataResult, RxDropReason, receive_station_data};
pub use sa_query::{
    SaQueryError, SaQueryOutcome, SaQueryState, receive_sa_query_request, receive_sa_query_response,
};
pub use scan::{
    BackgroundScanEffects, BackgroundScanPolicy, BeginScanEffects, EndScanEffects, EndScanPolicy,
    ScanError, ScanProgress, ScanStep, background_scan_timeout, begin_background_scan, begin_scan,
    end_station_scan, next_scan_channel, reset_scan_channels,
};
