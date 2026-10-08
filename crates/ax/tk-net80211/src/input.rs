//! 802.11 input-header metadata helpers from OpenBSD net80211.
//!
//! Translated from `sys/net80211/ieee80211_input.c` rev 1.263 and inline
//! helpers in `ieee80211.h` rev 1.137 (BSD-3-Clause). Copyright (c) 2001
//! Atsushi Onoe; Copyright (c) 2002, 2003 Sam Leffler, Errno Consulting;
//! Copyright (c) 2007-2009 Damien Bergamini.

const FC0_TYPE_MASK: u8 = 0x0c;
const FC0_TYPE_MGT: u8 = 0x00;
const FC0_TYPE_CTL: u8 = 0x04;
const FC0_TYPE_DATA: u8 = 0x08;
const FC0_SUBTYPE_QOS: u8 = 0x80;
const FC1_DIR_MASK: u8 = 0x03;
const FC1_DIR_DSTODS: u8 = 0x03;
const FC1_ORDER: u8 = 0x80;
const MAC_HEADER_LEN: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    ShortFrameControl,
    ControlFrameHasNoSequence,
    TruncatedHeader,
    NotQosData,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdcaAcParams {
    pub admission_control_mandatory: bool,
    pub aifsn: u8,
    pub ecw_min: u8,
    pub ecw_max: u8,
    pub txop_limit: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdcaState {
    pub update_count: u8,
    /// Records remain in OpenBSD AC order: BE, BK, VI, VO.
    pub access_categories: [EdcaAcParams; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdcaError {
    InvalidInformationElement,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdcaUpdate {
    pub changed: bool,
    pub notify_driver: bool,
}

// upstream: ieee80211_node.h ieee80211_has_seq()
pub fn has_sequence_control(fc0: u8) -> bool {
    fc0 & FC0_TYPE_MASK != FC0_TYPE_CTL
}

// upstream: ieee80211_node.h ieee80211_has_addr4()
pub fn has_address4(fc1: u8) -> bool {
    fc1 & FC1_DIR_MASK == FC1_DIR_DSTODS
}

// upstream: ieee80211_node.h ieee80211_has_qos()
pub fn has_qos_control(fc0: u8) -> bool {
    fc0 & (FC0_TYPE_MASK | FC0_SUBTYPE_QOS) == FC0_TYPE_DATA | FC0_SUBTYPE_QOS
}

// upstream: ieee80211_node.h ieee80211_has_htc()
pub fn has_ht_control(fc0: u8, fc1: u8) -> bool {
    fc1 & FC1_ORDER != 0 && (has_qos_control(fc0) || fc0 & FC0_TYPE_MASK == FC0_TYPE_MGT)
}

/// Retrieve the source header byte count (control frames are intentionally unsupported).
// upstream: ieee80211_input.c ieee80211_get_hdrlen()
pub fn header_length(frame: &[u8]) -> Result<usize, HeaderError> {
    if frame.len() < 2 {
        return Err(HeaderError::ShortFrameControl);
    }
    let fc0 = frame[0];
    let fc1 = frame[1];
    if !has_sequence_control(fc0) {
        return Err(HeaderError::ControlFrameHasNoSequence);
    }
    let len = MAC_HEADER_LEN
        + if has_address4(fc1) { 6 } else { 0 }
        + if has_qos_control(fc0) { 2 } else { 0 }
        + if has_ht_control(fc0, fc1) { 4 } else { 0 };
    if frame.len() < len {
        return Err(HeaderError::TruncatedHeader);
    }
    Ok(len)
}

/// Return the QoS control field in host little-endian bit order.
// upstream: ieee80211.h ieee80211_get_qos()
pub fn qos_control(frame: &[u8]) -> Result<u16, HeaderError> {
    if frame.len() < 2 {
        return Err(HeaderError::ShortFrameControl);
    }
    if !has_qos_control(frame[0]) {
        return Err(HeaderError::NotQosData);
    }
    let offset = MAC_HEADER_LEN + if has_address4(frame[1]) { 6 } else { 0 };
    let bytes = frame
        .get(offset..offset + 2)
        .ok_or(HeaderError::TruncatedHeader)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Parse an EDCA parameter body and detect 4-bit update-count changes.
// upstream: ieee80211_input.c ieee80211_parse_edca_params_body()
pub fn parse_edca_body(
    state: &mut EdcaState,
    body: &[u8],
    qos_enabled: bool,
) -> Result<EdcaUpdate, EdcaError> {
    if body.len() < 18 {
        return Err(EdcaError::InvalidInformationElement);
    }
    let update_count = body[0] & 0x0f;
    if update_count == state.update_count {
        return Ok(EdcaUpdate::default());
    }
    state.update_count = update_count;
    for (index, ac) in state.access_categories.iter_mut().enumerate() {
        let offset = 2 + index * 4;
        ac.admission_control_mandatory = body[offset] & 0x10 != 0;
        ac.aifsn = body[offset] & 0x0f;
        ac.ecw_min = body[offset + 1] & 0x0f;
        ac.ecw_max = body[offset + 1] >> 4;
        ac.txop_limit = u16::from_le_bytes([body[offset + 2], body[offset + 3]]);
    }
    Ok(EdcaUpdate {
        changed: true,
        notify_driver: qos_enabled,
    })
}

/// Parse an EDCA Parameter Set information element.
// upstream: ieee80211_input.c ieee80211_parse_edca_params()
pub fn parse_edca_ie(
    state: &mut EdcaState,
    ie: &[u8],
    qos_enabled: bool,
) -> Result<EdcaUpdate, EdcaError> {
    if ie.len() < 2 || usize::from(ie[1]) < 18 || ie.len() < 20 {
        return Err(EdcaError::InvalidInformationElement);
    }
    parse_edca_body(state, &ie[2..], qos_enabled)
}

/// Parse a WMM Parameter IE, preserving its source fixed-header offset.
// upstream: ieee80211_input.c ieee80211_parse_wmm_params()
pub fn parse_wmm_params(
    state: &mut EdcaState,
    ie: &[u8],
    qos_enabled: bool,
) -> Result<EdcaUpdate, EdcaError> {
    if ie.len() < 2 || usize::from(ie[1]) < 24 || ie.len() < 26 {
        return Err(EdcaError::InvalidInformationElement);
    }
    parse_edca_body(state, &ie[8..], qos_enabled)
}

/// Return the QoS Info byte in a WMM information/parameter element.
// upstream: ieee80211_input.c ieee80211_parse_wmm_qosinfo()
pub fn parse_wmm_qos_info(ie: &[u8]) -> Result<u8, EdcaError> {
    if ie.len() < 2 || usize::from(ie[1]) < 7 || ie.len() < 9 {
        return Err(EdcaError::InvalidInformationElement);
    }
    Ok(ie[8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_size_uses_addr4_qos_and_ht_control_combinations() {
        let mut base = [0u8; 36];
        assert_eq!(header_length(&base[..24]), Ok(24));
        base[0] = FC0_TYPE_DATA | FC0_SUBTYPE_QOS;
        assert!(has_qos_control(base[0]));
        assert_eq!(header_length(&base[..26]), Ok(26));
        base[1] = FC1_DIR_DSTODS | FC1_ORDER;
        assert!(has_address4(base[1]));
        assert!(has_ht_control(base[0], base[1]));
        assert_eq!(header_length(&base[..36]), Ok(36));
        base[0] = FC0_TYPE_MGT;
        base[1] = FC1_ORDER;
        assert_eq!(header_length(&base[..28]), Ok(28)); // management HTC
    }

    #[test]
    fn control_qos_and_truncated_frames_are_rejected() {
        assert_eq!(
            header_length(&[FC0_TYPE_CTL, 0]),
            Err(HeaderError::ControlFrameHasNoSequence)
        );
        assert_eq!(header_length(&[0]), Err(HeaderError::ShortFrameControl));
        assert_eq!(
            header_length(&[FC0_TYPE_DATA | FC0_SUBTYPE_QOS, 0]),
            Err(HeaderError::TruncatedHeader)
        );
        let mut qos = [0u8; 26];
        qos[0] = FC0_TYPE_DATA | FC0_SUBTYPE_QOS;
        qos[24] = 0x34;
        qos[25] = 0x12;
        assert_eq!(qos_control(&qos), Ok(0x1234));
        assert_eq!(qos_control(&[0; 26]), Err(HeaderError::NotQosData));
    }

    #[test]
    fn edca_wmm_parameter_updates_follow_update_count_and_field_order() {
        let mut state = EdcaState::default();
        let mut body = [0u8; 18];
        body[0] = 1;
        body[2] = 0x1f;
        body[3] = 0x43;
        body[4] = 0x34;
        body[5] = 0x12;
        let update = parse_edca_body(&mut state, &body, true).unwrap();
        assert_eq!(
            update,
            EdcaUpdate {
                changed: true,
                notify_driver: true
            }
        );
        assert_eq!(
            state.access_categories[0],
            EdcaAcParams {
                admission_control_mandatory: true,
                aifsn: 15,
                ecw_min: 3,
                ecw_max: 4,
                txop_limit: 0x1234
            }
        );
        assert!(!parse_edca_body(&mut state, &body, true).unwrap().changed);
        assert_eq!(
            parse_wmm_qos_info(&[221, 7, 0, 0, 0, 2, 1, 1, 0x8f]),
            Ok(0x8f)
        );
        let mut wmm = [0u8; 26];
        wmm[1] = 24;
        wmm[8] = 2;
        wmm[10] = 0x1f;
        assert!(parse_wmm_params(&mut state, &wmm, false).unwrap().changed);
        assert_eq!(state.access_categories[0].admission_control_mandatory, true);
    }
}
