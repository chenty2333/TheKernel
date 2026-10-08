//! Station association response processing from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263,
//! `ieee80211_proto.c` rev 1.176, `ieee80211_node.c` rev 1.217, and
//! `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001 Atsushi Onoe;
//! Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting; Copyright (c)
//! 2007-2009 Damien Bergamini.

use crate::{
    CAPINFO_SHORT_PREAMBLE, CAPINFO_SHORT_SLOTTIME, FixRateConfig, LocalPhyConfig, NodeTable,
    PeerRateState, PhyMode, ProtocolState, RATE_BASIC, RATE_MAX_SIZE, RateIeError, RateSet,
    fix_rate, node_abg_mode, node_is_11g, parse_edca_ie, parse_wmm_params, parse_wmm_qos_info,
    setup_he_caps, setup_he_operation, setup_ht_caps, setup_ht_operation, setup_rates,
    setup_vht_caps, setup_vht_operation,
};

const MAC_HEADER_LEN: usize = 24;
const ASSOC_FIXED_LEN: usize = 6;
const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_SUBTYPE_MASK: u8 = 0xf0;
const FC0_SUBTYPE_ASSOC_RESP: u8 = 0x10;
const FC0_SUBTYPE_REASSOC_RESP: u8 = 0x30;
const EID_RATES: u8 = 1;
const EID_EDCA: u8 = 12;
const EID_XRATES: u8 = 50;
const EID_HT_CAPS: u8 = 45;
const EID_HT_OP: u8 = 61;
const EID_VHT_CAPS: u8 = 191;
const EID_VHT_OP: u8 = 192;
const EID_EXTENSION: u8 = 255;
const EXT_HE_CAPS: u8 = 35;
const EXT_HE_OP: u8 = 36;
const EID_VENDOR: u8 = 221;
const WPA_OUI: [u8; 3] = [0x00, 0x50, 0xf2];
const WMM_AP_UAPSD: u8 = 0x80;
const ERP_USE_PROTECTION: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssocRxError {
    ShortFrame,
    NotAssociationResponse,
    NotStationAssociating,
    InvalidSupportedRates,
    RateIe(RateIeError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssocRxPolicy<'a> {
    pub station_mode: bool,
    pub state: ProtocolState,
    pub reassociation: bool,
    pub local_rates: &'a RateSet,
    pub fixed_rate: Option<usize>,
    pub local_phy: LocalPhyConfig,
    pub local_channel_160_allowed: bool,
    pub pairwise_ciphers: u32,
    pub rsn_enabled: bool,
    pub wep_enabled: bool,
    pub local_qos_enabled: bool,
    pub local_uapsd_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssocRxResult {
    pub status: u16,
    pub association_id: u16,
    pub rates_status: u8,
    pub malformed_ie_count: u32,
    pub qos: bool,
    pub uapsd: bool,
    pub negotiated_mode: PhyMode,
    pub erp_protection: bool,
    pub short_slot: bool,
    pub short_preamble: bool,
    pub protected_port_wait: bool,
    pub new_state: Option<ProtocolState>,
}

#[derive(Default)]
struct AssocIes<'a> {
    rates: Option<&'a [u8]>,
    extended_rates: Option<&'a [u8]>,
    edca: Option<&'a [u8]>,
    wmm: Option<&'a [u8]>,
    htcaps: Option<&'a [u8]>,
    htop: Option<&'a [u8]>,
    vhtcaps: Option<&'a [u8]>,
    vhtop: Option<&'a [u8]>,
    hecaps: Option<&'a [u8]>,
    heop: Option<&'a [u8]>,
}

fn parse_assoc_ies(mut bytes: &[u8]) -> (AssocIes<'_>, u32) {
    let mut ies = AssocIes::default();
    let mut malformed = 0;
    while bytes.len() >= 2 {
        let len = usize::from(bytes[1]);
        let Some(end) = 2usize.checked_add(len).filter(|&end| end <= bytes.len()) else {
            malformed += 1;
            break;
        };
        let element = &bytes[..end];
        let payload = &element[2..];
        match element[0] {
            EID_RATES => ies.rates = Some(element),
            EID_XRATES => ies.extended_rates = Some(element),
            EID_EDCA => ies.edca = Some(element),
            EID_HT_CAPS => ies.htcaps = Some(element),
            EID_HT_OP => ies.htop = Some(element),
            EID_VHT_CAPS => ies.vhtcaps = Some(element),
            EID_VHT_OP => ies.vhtop = Some(element),
            EID_EXTENSION if !payload.is_empty() => match payload[0] {
                EXT_HE_CAPS => ies.hecaps = Some(element),
                EXT_HE_OP => ies.heop = Some(element),
                _ => {}
            },
            EID_EXTENSION => malformed += 1,
            EID_VENDOR => {
                if payload.len() < 4 {
                    malformed += 1;
                } else if payload[..3] == WPA_OUI
                    && payload.len() >= 5
                    && payload[3] == 2
                    && payload[4] == 1
                {
                    ies.wmm = Some(element);
                }
            }
            _ => {}
        }
        bytes = &bytes[end..];
    }
    if !bytes.is_empty() && bytes.len() < 2 {
        malformed += 1;
    }
    (ies, malformed)
}

/// Apply a station-mode Association/Reassociation Response to the BSS node.
// upstream: ieee80211_input.c ieee80211_recv_assoc_resp()
pub fn receive_assoc_response(
    table: &mut NodeTable,
    frame: &[u8],
    policy: &AssocRxPolicy<'_>,
) -> Result<AssocRxResult, AssocRxError> {
    if frame.len() < MAC_HEADER_LEN + ASSOC_FIXED_LEN {
        return Err(AssocRxError::ShortFrame);
    }
    let subtype = frame[0] & FC0_SUBTYPE_MASK;
    if frame[0] & FC0_TYPE_MASK != FC0_TYPE_MGT
        || !matches!(subtype, FC0_SUBTYPE_ASSOC_RESP | FC0_SUBTYPE_REASSOC_RESP)
    {
        return Err(AssocRxError::NotAssociationResponse);
    }
    if policy.reassociation != (subtype == FC0_SUBTYPE_REASSOC_RESP) {
        return Err(AssocRxError::NotAssociationResponse);
    }
    if !policy.station_mode || policy.state != ProtocolState::Assoc {
        return Err(AssocRxError::NotStationAssociating);
    }
    let body = &frame[MAC_HEADER_LEN..MAC_HEADER_LEN + ASSOC_FIXED_LEN];
    let capability = u16::from_le_bytes([body[0], body[1]]);
    let status = u16::from_le_bytes([body[2], body[3]]);
    let association_id = u16::from_le_bytes([body[4], body[5]]);
    let (ies, malformed_ie_count) = parse_assoc_ies(&frame[MAC_HEADER_LEN + ASSOC_FIXED_LEN..]);
    if status != 0 {
        return Ok(AssocRxResult {
            status,
            association_id,
            rates_status: 0,
            malformed_ie_count,
            qos: false,
            uapsd: false,
            negotiated_mode: PhyMode::Auto,
            erp_protection: false,
            short_slot: false,
            short_preamble: false,
            protected_port_wait: false,
            new_state: None,
        });
    }
    let rates = ies.rates.ok_or(AssocRxError::InvalidSupportedRates)?;
    if rates.len() < 2
        || usize::from(rates[1]) > RATE_MAX_SIZE
        || usize::from(rates[1]) > rates.len() - 2
    {
        return Err(AssocRxError::InvalidSupportedRates);
    }

    let mut peer_rates = PeerRateState {
        rates: table.bss_node.access_point.rates,
        ..Default::default()
    };
    let rates_status = setup_rates(
        &mut peer_rates,
        rates,
        ies.extended_rates,
        policy.local_phy.channel_is_2ghz,
        |set| {
            i32::from(fix_rate(
                set,
                FixRateConfig {
                    supported_rates: policy.local_rates,
                    fixed_rate: policy.fixed_rate,
                    hostap_mode: false,
                },
                crate::FIX_RATE_SORT
                    | crate::FIX_RATE_FIXED
                    | crate::FIX_RATE_NEGOTIATE
                    | crate::FIX_RATE_DELETE,
            ))
        },
    )
    .map_err(AssocRxError::RateIe)? as u8;
    if rates_status & RATE_BASIC != 0 {
        return Ok(AssocRxResult {
            status,
            association_id,
            rates_status,
            malformed_ie_count,
            qos: false,
            uapsd: false,
            negotiated_mode: PhyMode::Auto,
            erp_protection: false,
            short_slot: false,
            short_preamble: false,
            protected_port_wait: false,
            new_state: None,
        });
    }

    let node = &mut table.bss_node;
    node.association_id = association_id;
    node.access_point.capability_info = capability;
    node.access_point.rates = peer_rates.rates;
    let wmm_qos_info = ies.wmm.and_then(|ie| parse_wmm_qos_info(ie).ok());
    let qos = if ies.edca.is_some() || ies.wmm.is_some() {
        let edca_ok = ies.edca.is_some_and(|edca| {
            parse_edca_ie(&mut node.edca, edca, policy.local_qos_enabled).is_ok()
        });
        let wmm_ok = !edca_ok
            && ies.wmm.is_some_and(|wmm| {
                parse_wmm_params(&mut node.edca, wmm, policy.local_qos_enabled).is_ok()
            });
        edca_ok || wmm_ok
    } else {
        node.qos
    };
    node.qos = qos;
    node.uapsd = policy.local_uapsd_enabled
        && qos
        && wmm_qos_info.is_some_and(|info| info & WMM_AP_UAPSD != 0);

    if let Some(htcaps) = ies.htcaps {
        let _ = setup_ht_caps(&mut node.ht_caps, &htcaps[2..]);
    }
    if let Some(htop) = ies.htop {
        let _ = setup_ht_operation(&mut node.ht_operation, &htop[2..], false);
    }
    if let (Some(_), Some(vhtcaps)) = (ies.htcaps, ies.vhtcaps)
        && policy.local_phy.channel_is_5ghz
    {
        let _ = setup_vht_caps(&mut node.vht_caps, &vhtcaps[2..]);
        if let Some(vhtop) = ies.vhtop {
            let _ = setup_vht_operation(
                &mut node.vht_operation,
                &node.ht_operation,
                node.vht_caps.caps,
                node.ht_operation.primary_channel,
                policy.local_channel_160_allowed,
                policy.local_phy.vht_caps.caps,
                policy.local_phy.vht_caps.tx_max_lgi_mbps,
                &vhtop[2..],
            );
        }
    }
    let peer = crate::PeerPhyConfig {
        ht_caps: node.ht_caps,
        ht_operation: node.ht_operation,
        vht_caps: node.vht_caps,
        vht_operation: node.vht_operation,
        he_caps: node.he_caps,
    };
    let ht = crate::ht_negotiate(&policy.local_phy, &peer, policy.pairwise_ciphers);
    let vht = crate::vht_negotiate(&policy.local_phy, &peer);
    if let Some(hecaps) = ies.hecaps {
        if hecaps.len() >= 3 {
            let _ = setup_he_caps(&mut node.he_caps, &hecaps[3..]);
        }
    }
    if let Some(heop) = ies.heop {
        if heop.len() >= 3 {
            let _ = setup_he_operation(&mut node.he_caps, &heop[3..], false);
        }
    }
    let peer = crate::PeerPhyConfig {
        ht_caps: node.ht_caps,
        ht_operation: node.ht_operation,
        vht_caps: node.vht_caps,
        vht_operation: node.vht_operation,
        he_caps: node.he_caps,
    };
    let he = crate::he_negotiate(&policy.local_phy, &peer, ht.0);
    let negotiated_mode = if he {
        PhyMode::Ax
    } else if vht.0 {
        PhyMode::Ac
    } else if ht.0 {
        PhyMode::N
    } else {
        node_abg_mode(
            None,
            policy.local_phy.channel_is_5ghz,
            if node_is_11g(&node.access_point.rates, policy.local_phy.channel_is_2ghz) {
                crate::NODE_CHAN_OFDM
            } else {
                0
            },
            0,
        )
    };
    let short_slot = negotiated_mode == PhyMode::A || capability & CAPINFO_SHORT_SLOTTIME != 0;
    let short_preamble = negotiated_mode == PhyMode::A || capability & CAPINFO_SHORT_PREAMBLE != 0;
    let erp_protection = matches!(negotiated_mode, PhyMode::G | PhyMode::N | PhyMode::Ax)
        && policy.local_phy.channel_is_2ghz
        && node.erp & ERP_USE_PROTECTION != 0;
    if policy.rsn_enabled {
        node.access_point.rsn_protocols = crate::PROTO_RSN;
    } else if policy.wep_enabled {
        node.access_point.rsn_protocols = 0;
    }
    Ok(AssocRxResult {
        status,
        association_id,
        rates_status,
        malformed_ie_count,
        qos,
        uapsd: node.uapsd,
        negotiated_mode,
        erp_protection,
        short_slot,
        short_preamble,
        protected_port_wait: policy.rsn_enabled,
        new_state: Some(ProtocolState::Run),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy<'a>(rates: &'a RateSet, state: ProtocolState) -> AssocRxPolicy<'a> {
        AssocRxPolicy {
            station_mode: true,
            state,
            reassociation: false,
            local_rates: rates,
            fixed_rate: None,
            local_phy: LocalPhyConfig {
                modecaps: 1 << PhyMode::B as u8,
                ht_enabled: false,
                vht_enabled: false,
                he_enabled: false,
                channel_is_2ghz: true,
                channel_is_5ghz: false,
                channel_supports_ac: false,
                channel_supports_he: false,
                station_mode: true,
                wep_enabled: false,
                rsn_enabled: false,
                ht_caps: crate::HtCapabilities::default(),
                supported_ht_mcs: [0; 10],
                vht_caps: crate::VhtCapabilities::default(),
                he_caps: crate::HeCapabilities::default(),
            },
            local_channel_160_allowed: false,
            pairwise_ciphers: 0,
            rsn_enabled: false,
            wep_enabled: false,
            local_qos_enabled: false,
            local_uapsd_enabled: false,
        }
    }

    fn response(status: u16) -> alloc::vec::Vec<u8> {
        let mut frame = alloc::vec![0; MAC_HEADER_LEN + ASSOC_FIXED_LEN];
        frame[0] = FC0_SUBTYPE_ASSOC_RESP;
        frame[MAC_HEADER_LEN..MAC_HEADER_LEN + 2]
            .copy_from_slice(&crate::CAPINFO_SHORT_SLOTTIME.to_le_bytes());
        frame[MAC_HEADER_LEN + 2..MAC_HEADER_LEN + 4].copy_from_slice(&status.to_le_bytes());
        frame[MAC_HEADER_LEN + 4..MAC_HEADER_LEN + 6].copy_from_slice(&0xc001u16.to_le_bytes());
        frame.extend_from_slice(&[EID_RATES, 4, 0x82, 0x84, 0x8b, 0x96]);
        frame
    }

    #[test]
    fn successful_response_commits_bss_and_returns_run_transition() {
        let mut table = NodeTable::default();
        let rates = RateSet::new(&[2, 4, 11, 22]);
        let result = receive_assoc_response(
            &mut table,
            &response(0),
            &policy(&rates, ProtocolState::Assoc),
        )
        .unwrap();
        assert_eq!(result.new_state, Some(ProtocolState::Run));
        assert_eq!(result.association_id, 0xc001);
        assert!(result.short_slot);
        assert_eq!(table.bss_node.association_id, 0xc001);
    }

    #[test]
    fn unsuccessful_response_is_reported_without_installing_association() {
        let mut table = NodeTable::default();
        let rates = RateSet::new(&[2, 4, 11, 22]);
        let result = receive_assoc_response(
            &mut table,
            &response(17),
            &policy(&rates, ProtocolState::Assoc),
        )
        .unwrap();
        assert_eq!(result.status, 17);
        assert_eq!(result.new_state, None);
        assert_eq!(table.bss_node.association_id, 0);
        assert_eq!(table.bss_node.access_point.association_failures, 0);
    }

    #[test]
    fn rejects_wrong_state_and_short_frame() {
        let mut table = NodeTable::default();
        let rates = RateSet::new(&[2, 4, 11, 22]);
        let frame = response(0);
        assert_eq!(
            receive_assoc_response(
                &mut table,
                &frame[..MAC_HEADER_LEN],
                &policy(&rates, ProtocolState::Assoc)
            ),
            Err(AssocRxError::ShortFrame)
        );
        assert_eq!(
            receive_assoc_response(&mut table, &frame, &policy(&rates, ProtocolState::Auth)),
            Err(AssocRxError::NotStationAssociating)
        );
    }
}
