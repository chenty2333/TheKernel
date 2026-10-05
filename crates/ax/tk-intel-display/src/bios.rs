// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_bios.c: intel_bios_is_valid_vbt,
// _get_blocksize/find_raw_section, child_device_expected_size/parse_general_definitions,
// dvo_port_to_port (XELPD mapping), map_ddc_pin (ADL-P mapping), encoder_supports_*,
// intel_bios_hdmi_max_tmds_clock and sanitize_dedicated_external.
// Copyright © 2006 Intel Corporation.
// intel_vbt_defs.h: vbt_header/bdb_header/bdb_general_definitions/child_device_config.
// Copyright © 2006-2016 Intel Corporation. MIT permission text: ../LICENSE-MIT.
// Pre-216/after-264 BDB semantic versions, other platforms, LVDS/DSI/panel blocks
// and unused child fields omitted. Checked byte access replaces C packed loads.
use crate::{Error, bytes, device::Port, le16, le32};

#[derive(Clone, Copy, Debug)]
pub struct Vbt<'a> {
    data: &'a [u8],
    bdb: &'a [u8],
    header_size: usize,
    pub version: u16,
}
impl<'a> Vbt<'a> {
    /// Same VBT/BDB extent validation as i915, plus minimum/header/signature
    /// checks before iteration. Sections are checked lazily like upstream; an
    /// unrelated malformed OEM tail does not discard an earlier valid block. No packed references or unaligned integer loads.
    /// Like i915, a bad checksum is not automatically a parse failure; admission
    /// policy can inspect checksum_valid without changing parser semantics.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        bytes(data, 0, 48)?;
        if &data[..4] != b"$VBT" {
            return Err(Error::InvalidHeader);
        }
        let size = usize::from(le16(data, 24)?);
        let header = usize::from(le16(data, 22)?);
        if header < 48 || header > size {
            return Err(Error::InvalidHeader);
        }
        let data = bytes(data, 0, size)?;
        let bdb_offset = usize::try_from(le32(data, 28)?).map_err(|_| Error::Truncated)?;
        if bdb_offset < header {
            return Err(Error::InvalidHeader);
        }
        let bdb = bytes(data, bdb_offset, 22)?;
        if &bdb[..16] != b"BIOS_DATA_BLOCK " {
            return Err(Error::InvalidHeader);
        }
        let bdb_size = usize::from(le16(bdb, 20)?);
        let header_size = usize::from(le16(bdb, 18)?);
        if header_size < 22 || header_size > bdb_size {
            return Err(Error::InvalidHeader);
        }
        let bdb = bytes(data, bdb_offset, bdb_size)?;
        let vbt = Self {
            data,
            bdb,
            header_size,
            version: le16(bdb, 16)?,
        };
        Ok(vbt)
    }
    pub fn data(&self) -> &'a [u8] {
        self.data
    }
    pub fn checksum_valid(&self) -> bool {
        self.data.iter().fold(0u8, |a, b| a.wrapping_add(*b)) == 0
    }
    pub fn sections(&self) -> Sections<'a> {
        Sections {
            remaining: &self.bdb[self.header_size..],
        }
    }
    pub fn find_raw_section(&self, id: u8) -> Result<Option<&'a [u8]>, Error> {
        for section in self.sections() {
            let (current, data) = section?;
            if current == id {
                return Ok(Some(data));
            }
        }
        Ok(None)
    }
    pub fn parse_general_definitions(&self) -> Result<GeneralDefinitions<'a>, Error> {
        if !(216..=264).contains(&self.version) {
            return Err(Error::UnsupportedVersion);
        }
        let data = self.find_raw_section(2)?.ok_or(Error::InvalidBlock)?;
        bytes(data, 0, 5)?;
        let size = usize::from(data[4]);
        // i915's minimum is the legacy struct, zero-extending short children.
        if size < 33 || !(data.len() - 5).is_multiple_of(size) {
            return Err(Error::InvalidBlock);
        }
        Ok(GeneralDefinitions {
            crt_ddc_pin: data[0],
            record_size: size,
            record_size_expected: size == child_device_expected_size(self.version),
            version: self.version,
            children: &data[5..],
        })
    }
}

pub struct Sections<'a> {
    remaining: &'a [u8],
}
impl<'a> Iterator for Sections<'a> {
    type Item = Result<(u8, &'a [u8]), Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let result = (|| {
            let header = bytes(self.remaining, 0, 3)?;
            let id = header[0];
            let size = if id == 53 && *bytes(self.remaining, 3, 1)?.first().unwrap() >= 3 {
                usize::try_from(le32(self.remaining, 4)?).map_err(|_| Error::Truncated)?
            } else {
                usize::from(le16(header, 1)?)
            };
            let data = bytes(self.remaining, 3, size)?;
            self.remaining = &self.remaining[3 + size..];
            Ok((id, data))
        })();
        if result.is_err() {
            self.remaining = &[];
        }
        Some(result)
    }
}
fn child_device_expected_size(version: u16) -> usize {
    if version >= 263 {
        44
    } else if version >= 256 {
        40
    } else {
        39
    }
}

pub struct GeneralDefinitions<'a> {
    pub crt_ddc_pin: u8,
    pub record_size: usize,
    pub record_size_expected: bool,
    version: u16,
    children: &'a [u8],
}
impl GeneralDefinitions<'_> {
    /// Empty child records are omitted, in upstream list order. Unknown ports
    /// remain visible as None; parsing is not a claim that the PHY is present.
    pub fn children(&self) -> impl Iterator<Item = ChildDevice> + '_ {
        self.children
            .chunks_exact(self.record_size)
            .filter_map(|record| {
                let device_type = u16::from_le_bytes([record[2], record[3]]);
                if device_type == 0 {
                    return None;
                }
                let byte = |index| record.get(index).copied().unwrap_or(0);
                let dedicated = self.version >= 264 && byte(33) & 4 != 0;
                Some(ChildDevice {
                    handle: u16::from_le_bytes([record[0], record[1]]),
                    device_type,
                    dvo_port: byte(16),
                    port: dvo_port_to_port(byte(16)),
                    ddc_pin: byte(19),
                    aux_channel: byte(25),
                    hdmi_level_shift: byte(7) & 0x1f,
                    hdmi_max_tmds_khz: hdmi_max_tmds_clock(byte(7) >> 5),
                    lane_reversal: byte(23) & 2 != 0,
                    hpd_invert: byte(23) & 16 != 0,
                    use_vbt_vswing: byte(23) & 32 != 0,
                    lspcon: byte(23) & 4 != 0,
                    dp_max_lane_count: if self.version >= 244 {
                        (byte(23) >> 6) + 1
                    } else {
                        0
                    },
                    usb_type_c: byte(33) & 1 != 0 && !dedicated,
                    thunderbolt: byte(33) & 2 != 0 && !dedicated,
                    dedicated_external: dedicated,
                    dynamic_port_over_tc: self.version >= 264 && byte(33) & 8 != 0 && !dedicated,
                })
            })
    }
    /// Hardware admission must not choose the first of contradictory records.
    /// This is deliberately stricter than Linux's first-record lookup.
    pub fn encoder(&self, port: Port) -> Result<Option<ChildDevice>, Error> {
        let mut matches = self.children().filter(|child| child.port == Some(port));
        let first = matches.next();
        if matches.next().is_some() {
            return Err(Error::InvalidBlock);
        }
        Ok(first)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildDevice {
    pub handle: u16,
    pub device_type: u16,
    pub dvo_port: u8,
    pub port: Option<Port>,
    /// Raw BIOS DDC pin, NOT the hardware GMBUS selector.
    pub ddc_pin: u8,
    pub aux_channel: u8,
    pub hdmi_level_shift: u8,
    pub hdmi_max_tmds_khz: u32,
    pub lane_reversal: bool,
    pub hpd_invert: bool,
    pub use_vbt_vswing: bool,
    pub lspcon: bool,
    pub dp_max_lane_count: u8,
    pub usb_type_c: bool,
    pub thunderbolt: bool,
    pub dedicated_external: bool,
    pub dynamic_port_over_tc: bool,
}
impl ChildDevice {
    pub const fn supports_dvi(self) -> bool {
        self.device_type & (1 << 4) != 0
    }
    pub const fn supports_hdmi(self) -> bool {
        self.supports_dvi() && self.device_type & (1 << 11) == 0
    }
    pub const fn supports_dp(self) -> bool {
        self.device_type & (1 << 2) != 0
    }
    pub const fn supports_edp(self) -> bool {
        self.supports_dp() && self.device_type & (1 << 12) != 0
    }
    pub const fn gmbus_pin(self) -> Option<u8> {
        map_ddc_pin(self.ddc_pin)
    }
}

/// Display 13 XELPD mapping, not the old pre-display-13 port-letter mapping.
pub const fn dvo_port_to_port(dvo: u8) -> Option<Port> {
    match dvo {
        0 | 10 => Some(Port::A),
        1 | 7 => Some(Port::B),
        2 | 8 => Some(Port::C),
        3 | 9 => Some(Port::D),
        12 | 11 => Some(Port::E),
        14 | 13 => Some(Port::Tc1),
        16 | 15 => Some(Port::Tc2),
        18 | 17 => Some(Port::Tc3),
        20 | 19 => Some(Port::Tc4),
        _ => None,
    }
}
/// ADL-P BIOS DDC bus 3 is GMBUS 9 (TC1), never GMBUS 3 (combo C).
pub const fn map_ddc_pin(vbt_pin: u8) -> Option<u8> {
    match vbt_pin {
        1 => Some(1),
        2 => Some(2),
        3 => Some(9),
        4 => Some(10),
        5 => Some(11),
        6 => Some(12),
        _ => None,
    }
}
pub const fn hdmi_max_tmds_clock(rate: u8) -> u32 {
    match rate {
        1 => 297000,
        2 => 165000,
        3 => 594000,
        4 => 340000,
        5 => 300000,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{vec, vec::Vec};

    use super::*;
    fn table(children: &[u8], stride: u8) -> Vec<u8> {
        let n = 48 + 22 + 3 + 5 + children.len();
        let mut data = vec![0; n];
        data[..4].copy_from_slice(b"$VBT");
        data[22..24].copy_from_slice(&48u16.to_le_bytes());
        data[24..26].copy_from_slice(&(n as u16).to_le_bytes());
        data[28..32].copy_from_slice(&48u32.to_le_bytes());
        data[48..64].copy_from_slice(b"BIOS_DATA_BLOCK ");
        data[64..66].copy_from_slice(&249u16.to_le_bytes());
        data[66..68].copy_from_slice(&22u16.to_le_bytes());
        data[68..70].copy_from_slice(&((n - 48) as u16).to_le_bytes());
        data[70] = 2;
        data[71..73].copy_from_slice(&((5 + children.len()) as u16).to_le_bytes());
        data[77] = stride;
        data[78..].copy_from_slice(children);
        data
    }
    fn tc_hdmi() -> [u8; 39] {
        let mut child = [0; 39];
        child[0] = 64;
        child[2..4].copy_from_slice(&0x60d2u16.to_le_bytes());
        child[16] = 14;
        child[19] = 3;
        child[7] = 5;
        child
    }
    #[test]
    fn legacy_hdmi_on_tc1_is_not_combo_or_usb_type_c() {
        let data = table(&tc_hdmi(), 39);
        let vbt = Vbt::parse(&data).unwrap();
        let defs = vbt.parse_general_definitions().unwrap();
        let tc = defs.encoder(Port::Tc1).unwrap().unwrap();
        assert_eq!(tc.gmbus_pin(), Some(9));
        assert!(tc.supports_hdmi());
        assert!(!tc.usb_type_c && !tc.thunderbolt && !tc.supports_dp());
        assert_eq!(tc.hdmi_level_shift, 5);
        assert!(defs.encoder(Port::A).unwrap().is_none());
    }
    #[test]
    fn every_truncation_and_bad_extent_fails_without_panic() {
        let data = table(&tc_hdmi(), 39);
        for end in 0..data.len() {
            assert!(Vbt::parse(&data[..end]).is_err(), "{end}");
        }
        for offset in [22, 24, 28, 66, 68] {
            let mut bad = data.clone();
            bad[offset..offset + 2].fill(0xff);
            assert!(Vbt::parse(&bad).is_err(), "{offset}");
        }
    }
    #[test]
    fn requested_block_checks_bounds_but_unused_tail_does_not_discard_it() {
        let mut data = table(&tc_hdmi(), 39);
        data[71..73].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(
            Vbt::parse(&data)
                .unwrap()
                .parse_general_definitions()
                .is_err()
        );
        let mut data = table(&tc_hdmi(), 39);
        data.extend_from_slice(&[250, 10, 0, 0]);
        let n = data.len();
        data[24..26].copy_from_slice(&(n as u16).to_le_bytes());
        data[68..70].copy_from_slice(&((n - 48) as u16).to_le_bytes());
        let vbt = Vbt::parse(&data).unwrap();
        assert!(vbt.parse_general_definitions().is_ok());
        assert!(vbt.find_raw_section(250).is_err());
    }
    #[test]
    fn zero_stride_partial_child_and_duplicate_port_refused() {
        assert!(
            Vbt::parse(&table(&tc_hdmi(), 0))
                .unwrap()
                .parse_general_definitions()
                .is_err()
        );
        assert!(
            Vbt::parse(&table(&tc_hdmi()[..38], 39))
                .unwrap()
                .parse_general_definitions()
                .is_err()
        );
        let mut both = tc_hdmi().to_vec();
        both.extend_from_slice(&tc_hdmi());
        let data = table(&both, 39);
        let vbt = Vbt::parse(&data).unwrap();
        assert!(
            vbt.parse_general_definitions()
                .unwrap()
                .encoder(Port::Tc1)
                .is_err()
        );
    }
    #[test]
    fn absent_tail_is_zero_extended_but_reported_unexpected() {
        let data = table(&tc_hdmi()[..33], 33);
        let vbt = Vbt::parse(&data).unwrap();
        let defs = vbt.parse_general_definitions().unwrap();
        assert!(!defs.record_size_expected);
        assert_eq!(defs.children().count(), 1);
        assert!(!defs.children().next().unwrap().usb_type_c);
    }
    #[test]
    fn dedicated_external_sanitizes_typec_flags_at_264_only() {
        let mut child = [0; 44];
        child[..39].copy_from_slice(&tc_hdmi());
        child[33] = 15;
        let mut data = table(&child, 44);
        data[64..66].copy_from_slice(&264u16.to_le_bytes());
        let vbt = Vbt::parse(&data).unwrap();
        let tc = vbt
            .parse_general_definitions()
            .unwrap()
            .encoder(Port::Tc1)
            .unwrap()
            .unwrap();
        assert!(tc.dedicated_external);
        assert!(!tc.usb_type_c && !tc.thunderbolt && !tc.dynamic_port_over_tc);
        data[64..66].copy_from_slice(&263u16.to_le_bytes());
        let vbt = Vbt::parse(&data).unwrap();
        let tc = vbt
            .parse_general_definitions()
            .unwrap()
            .encoder(Port::Tc1)
            .unwrap()
            .unwrap();
        assert!(!tc.dedicated_external);
        assert!(tc.usb_type_c && tc.thunderbolt);
    }
    #[test]
    fn mipi_v3_size_field_is_not_the_short_block_size() {
        let mut data = table(&tc_hdmi(), 39);
        data[70] = 53;
        data[73] = 3;
        let size = (data.len() - 73) as u32;
        data[74..78].copy_from_slice(&size.to_le_bytes());
        let vbt = Vbt::parse(&data).unwrap();
        assert_eq!(
            vbt.find_raw_section(53).unwrap().unwrap().len(),
            size as usize
        );
        data[74..78].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Vbt::parse(&data).unwrap().find_raw_section(53).is_err());
    }
    #[test]
    fn all_xelpd_port_pairs_and_invalid_ddc_are_explicit() {
        let pairs = [
            (0, 10, Port::A),
            (1, 7, Port::B),
            (2, 8, Port::C),
            (3, 9, Port::D),
            (12, 11, Port::E),
            (14, 13, Port::Tc1),
            (16, 15, Port::Tc2),
            (18, 17, Port::Tc3),
            (20, 19, Port::Tc4),
        ];
        for (hdmi, dp, port) in pairs {
            assert_eq!(dvo_port_to_port(hdmi), Some(port));
            assert_eq!(dvo_port_to_port(dp), Some(port));
        }
        for n in [4, 5, 6, 21, 255] {
            assert_eq!(dvo_port_to_port(n), None);
        }
        for pin in [0, 7, 8, 9, 255] {
            assert_eq!(map_ddc_pin(pin), None);
        }
    }
}
