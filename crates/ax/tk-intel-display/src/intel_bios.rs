// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_bios.c and
// intel_vbt_defs.h. Copyright © 2006 Intel Corporation (intel_bios.c);
// Copyright © 2006-2016 Intel Corporation (intel_vbt_defs.h).
// Copyright © 2025 Intel Corporation (intel_dsi_vbt_defs.h).
// Author: Eric Anholt <eric@anholt.net>. Full grants: ../LICENSE-MIT.
use crate::{Error, bytes, device::Port, le16, le32};

/// Read a BDB block's payload size from its three-byte block header.
// upstream: intel_bios.c _get_blocksize()
pub fn _get_blocksize(block_base: &[u8]) -> Result<usize, Error> {
    let id = *bytes(block_base, 0, 1)?.first().unwrap();
    if id == 53 && *bytes(block_base, 3, 1)?.first().unwrap() >= 3 {
        usize::try_from(le32(block_base, 4)?).map_err(|_| Error::Truncated)
    } else {
        Ok(usize::from(le16(block_base, 1)?))
    }
}

/// Find the first raw BDB payload with `section_id`, walking from header_size.
// upstream: intel_bios.c find_raw_section()
pub fn find_raw_section(
    bdb: &[u8],
    header_size: usize,
    section_id: u8,
) -> Result<Option<&[u8]>, Error> {
    let total = bdb.len();
    let mut index = header_size;
    while index.checked_add(3).is_some_and(|end| end < total) {
        let current_id = bdb[index];
        let current_size = _get_blocksize(&bdb[index..])?;
        index = index.checked_add(3).ok_or(Error::Truncated)?;
        let end = index.checked_add(current_size).ok_or(Error::Truncated)?;
        if end > total || end > bdb.len() {
            return Ok(None);
        }
        if current_id == section_id {
            let mipi_header = usize::from(current_id == 53 && bdb[index] >= 3) * 5;
            let data_end = end.checked_add(mipi_header).ok_or(Error::Truncated)?;
            if data_end > total {
                return Ok(None);
            }
            return Ok(Some(&bdb[index..data_end]));
        }
        index = end;
    }
    Ok(None)
}

/// Return a BDB-relative offset to a block payload, or zero when absent.
// upstream: intel_bios.c raw_block_offset()
pub fn raw_block_offset(bdb: &[u8], header_size: usize, section_id: u8) -> Result<usize, Error> {
    let Some(payload) = find_raw_section(bdb, header_size, section_id)? else {
        return Ok(0);
    };
    let start = bdb.as_ptr() as usize;
    let data = payload.as_ptr() as usize;
    data.checked_sub(start).ok_or(Error::InvalidBlock)
}

fn u16_zero_padded(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([
        data.get(offset).copied().unwrap_or(0),
        data.get(offset + 1).copied().unwrap_or(0),
    ])
}

fn u32_zero_padded(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data.get(offset).copied().unwrap_or(0),
        data.get(offset + 1).copied().unwrap_or(0),
        data.get(offset + 2).copied().unwrap_or(0),
        data.get(offset + 3).copied().unwrap_or(0),
    ])
}

fn u64_zero_padded(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        data.get(offset).copied().unwrap_or(0),
        data.get(offset + 1).copied().unwrap_or(0),
        data.get(offset + 2).copied().unwrap_or(0),
        data.get(offset + 3).copied().unwrap_or(0),
        data.get(offset + 4).copied().unwrap_or(0),
        data.get(offset + 5).copied().unwrap_or(0),
        data.get(offset + 6).copied().unwrap_or(0),
        data.get(offset + 7).copied().unwrap_or(0),
    ])
}

/// Return whether the supplied bytes contain the VBT and BDB extents i915 accepts.
// upstream: intel_bios.c intel_bios_is_valid_vbt()
pub fn intel_bios_is_valid_vbt(data: &[u8]) -> bool {
    if data.len() < 48 || &data[..4] != b"$VBT" {
        return false;
    }
    let Ok(vbt_size) = le16(data, 24) else {
        return false;
    };
    let vbt_size = usize::from(vbt_size);
    if vbt_size > data.len() {
        return false;
    }
    let Ok(bdb_offset) = le32(data, 28) else {
        return false;
    };
    let Ok(bdb_offset) = usize::try_from(bdb_offset) else {
        return false;
    };
    let Some(bdb_header_end) = bdb_offset.checked_add(22) else {
        return false;
    };
    if bdb_header_end > vbt_size {
        return false;
    }
    let Ok(bdb_size) = le16(data, bdb_offset + 20) else {
        return false;
    };
    bdb_offset
        .checked_add(usize::from(bdb_size))
        .is_some_and(|end| end <= vbt_size)
}

#[derive(Clone, Copy, Debug)]
pub struct Vbt<'a> {
    data: &'a [u8],
    bdb: &'a [u8],
    header_size: usize,
    pub version: u16,
}
impl<'a> Vbt<'a> {
    /// Construct a checked borrowed view after the source extent validation.
    /// No packed references or unaligned integer loads are formed.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        if !intel_bios_is_valid_vbt(data) {
            return Err(Error::InvalidHeader);
        }
        let size = usize::from(le16(data, 24)?);
        let data = bytes(data, 0, size)?;
        let bdb_offset = usize::try_from(le32(data, 28)?).map_err(|_| Error::Truncated)?;
        let bdb = bytes(data, bdb_offset, 22)?;
        let bdb_size = usize::from(le16(bdb, 20)?);
        let header_size = usize::from(le16(bdb, 18)?);
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
    pub fn find_raw_section(&self, id: u8) -> Result<Option<&'a [u8]>, Error> {
        find_raw_section(self.bdb, self.header_size, id)
    }
    /// Decode the optional display-13 AFC startup field after parsing the
    /// general-features structure.
    pub fn afc_startup_override(&self) -> Result<Option<u8>, Error> {
        let general = self.parse_general_features(13)?;
        if self.version >= 249 && general.afc_startup_config != 0 {
            Ok(Some(if general.afc_startup_config == 1 { 0 } else { 7 }))
        } else {
            Ok(None)
        }
    }
    /// Translate the BDB general-features block into the fields consumed by i915.
    // upstream: intel_bios.c parse_general_features()
    pub fn parse_general_features(
        &self,
        display_version: u8,
    ) -> Result<GeneralFeatures, Error> {
        let Some(raw) = self.find_raw_section(1)? else {
            return Ok(GeneralFeatures::default());
        };
        // init_bdb_block() zero-extends short blocks to the known structure size.
        let byte = |index: usize| raw.get(index).copied().unwrap_or(0);
        Ok(GeneralFeatures {
            enable_ssc: byte(1) & (1 << 1) != 0,
            ssc_frequency_khz: intel_bios_ssc_frequency(
                byte(1) & (1 << 2) != 0,
                display_version,
            ),
            display_clock_mode: byte(1) & (1 << 6) != 0,
            rotate_180: self.version >= 181 && byte(2) & (1 << 2) != 0,
            fdi_rx_polarity_inverted: byte(2) & (1 << 3) != 0,
            int_crt_support: self.version >= 155 && byte(4) & 1 != 0,
            int_tv_support: byte(4) & (1 << 1) != 0,
            afc_startup_config: if self.version >= 249 { byte(6) & 3 } else { 0 },
        })
    }
    /// Parse the driver-feature flags that affect display and panel support.
    // upstream: intel_bios.c parse_driver_features()
    pub fn parse_driver_features(
        &self,
        display_version: u8,
    ) -> Result<Option<DriverFeatures>, Error> {
        let Some(raw) = self.find_raw_section(12)? else {
            return Ok(None);
        };
        let driver_flags = u16_zero_padded(raw, 17);
        let lvds_config = ((u16_zero_padded(raw, 7) >> 10) & 3) as u8;
        let int_lvds_support = (display_version < 5)
            .then_some(lvds_config == 1 || (self.version >= 134 && lvds_config == 3))
            .unwrap_or(lvds_config == 1);
        Ok(Some(DriverFeatures {
            int_lvds_support,
            drrs_enabled: driver_flags & (1 << 5) != 0,
            psr_enabled: driver_flags & (1 << 9) != 0,
            dmrrs_enabled: driver_flags & (1 << 12) != 0,
        }))
    }
    /// Apply legacy driver-feature DRRS/PSR policy to panel defaults.
    // upstream: intel_bios.c parse_panel_driver_features()
    pub fn parse_panel_driver_features(
        &self,
        drrs_type: DrrsType,
        psr_enabled: bool,
    ) -> Result<PanelPowerFeatures, Error> {
        let mut result = PanelPowerFeatures {
            drrs_type,
            psr_enabled,
            vrr_enabled: true,
            hobl_enabled: false,
        };
        if self.version >= 228 {
            return Ok(result);
        }
        let Some(driver) = self.parse_driver_features(13)? else {
            return Ok(result);
        };
        if !driver.drrs_enabled && result.drrs_type != DrrsType::None {
            result.drrs_type = if driver.dmrrs_enabled {
                DrrsType::Static
            } else {
                DrrsType::None
            };
        }
        result.psr_enabled = driver.psr_enabled;
        Ok(result)
    }
    /// Apply panel power-conservation flags from BDB block 44.
    // upstream: intel_bios.c parse_power_conservation_features()
    pub fn parse_power_conservation_features(
        &self,
        panel_type: u8,
        mut result: PanelPowerFeatures,
    ) -> Result<PanelPowerFeatures, Error> {
        result.vrr_enabled = true;
        if self.version < 228 || panel_type >= 16 {
            return Ok(result);
        }
        let Some(power) = self.find_raw_section(44)? else {
            return Ok(result);
        };
        let bit = u32::from(panel_type);
        result.psr_enabled = (u16_zero_padded(power, 24) >> bit) & 1 != 0;
        if (u16_zero_padded(power, 26) >> bit) & 1 == 0
            && result.drrs_type != DrrsType::None
        {
            result.drrs_type = if (u16_zero_padded(power, 32) >> bit) & 1 != 0 {
                DrrsType::Static
            } else {
                DrrsType::None
            };
        }
        if self.version >= 232 {
            result.hobl_enabled = (u16_zero_padded(power, 54) >> bit) & 1 != 0;
        }
        if self.version >= 233 {
            result.vrr_enabled = (u16_zero_padded(power, 56) >> bit) & 1 != 0;
        }
        Ok(result)
    }
    /// Parse the active panel's MIPI configuration and PPS tables.
    // upstream: intel_bios.c parse_mipi_config()
    pub fn parse_mipi_config(
        &self,
        panel_type: u8,
        display_version: u8,
    ) -> Result<Option<MipiConfig>, Error> {
        if panel_type >= 16 {
            return Err(Error::InvalidBlock);
        }
        let definitions = self.parse_general_definitions()?;
        let Some(port) = definitions.is_dsi_present(display_version) else {
            return Ok(None);
        };
        let Some(raw) = self.find_raw_section(52)? else {
            return Ok(None);
        };
        let index = usize::from(panel_type);
        let config_offset = index * 122;
        let config = bytes(raw, config_offset, 122)?;
        let general = u32_zero_padded(config, 2);
        let port_desc = u16_zero_padded(config, 6);
        let dual_link = port_desc & 3 != 0;
        let cabc_supported = general & (1 << 8) != 0;
        let pps_offset = 732 + index * 10;
        let pwm_offset = 792 + index * 4;
        let pps = MipiPpsData {
            panel_on_delay: u16_zero_padded(raw, pps_offset),
            backlight_on_delay: u16_zero_padded(raw, pps_offset + 2),
            backlight_off_delay: u16_zero_padded(raw, pps_offset + 4),
            panel_off_delay: u16_zero_padded(raw, pps_offset + 6),
            panel_power_cycle_delay: u16_zero_padded(raw, pps_offset + 8),
        };
        let rotation = (general >> 14) & 3;
        let mut mipi_config = MipiConfig {
            panel_id: 1,
            port,
            command_mode: general & (1 << 5) != 0,
            dual_link,
            cabc_supported,
            backlight_ports: 0,
            cabc_ports: 0,
            orientation: match rotation {
                1 => PanelOrientation::RightUp,
                2 => PanelOrientation::BottomUp,
                3 => PanelOrientation::LeftUp,
                _ => PanelOrientation::Unknown,
            },
            pps,
            pwm_delays: [
                u16_zero_padded(raw, pwm_offset),
                u16_zero_padded(raw, pwm_offset + 2),
            ],
            pmic_i2c_bus_number: raw.get(816 + index).copied().unwrap_or(0),
            raw_config: config.try_into().unwrap(),
        };
        parse_dsi_backlight_ports(
            self.version,
            display_version,
            port_desc,
            port,
            &mut mipi_config,
        );
        Ok(Some(mipi_config))
    }
    /// Validate and index the selected panel's MIPI sequence block.
    // upstream: intel_bios.c parse_mipi_sequence()
    pub fn parse_mipi_sequence(
        &self,
        panel_type: u16,
        display_version: u8,
    ) -> Result<Option<MipiSequences<'a>>, Error> {
        if panel_type >= 16 {
            return Err(Error::InvalidBlock);
        }
        if self
            .parse_mipi_config(panel_type as u8, display_version)?
            .is_none()
        {
            return Ok(None);
        }
        let Some(raw) = self.find_raw_section(53)? else {
            return Ok(None);
        };
        let version = raw.first().copied().unwrap_or(0);
        if version >= 4 {
            return Err(Error::UnsupportedVersion);
        }
        let Some(sequence_data) = find_panel_sequence_block(raw, version, panel_type)? else {
            return Ok(None);
        };
        let mut sequences = [None; 12];
        let mut index = 0usize;
        loop {
            let sequence_id = *sequence_data.get(index).ok_or(Error::InvalidBlock)?;
            if sequence_id == 0 {
                break;
            }
            if usize::from(sequence_id) >= sequences.len() {
                return Err(Error::InvalidBlock);
            }
            let next = if version >= 3 {
                goto_next_sequence_v3(sequence_data, index)?
            } else {
                goto_next_sequence(sequence_data, index)?
            };
            sequences[usize::from(sequence_id)] = Some(&sequence_data[index..next]);
            index = next;
        }
        if display_version >= 11 && sequences[2].is_none() && sequences[3].is_some() {
            sequences.swap(2, 3);
        }
        Ok(Some(MipiSequences {
            version,
            sequence_data,
            sequences,
        }))
    }
    /// Parse BDB general definitions and child-device records.
    // upstream: intel_bios.c parse_general_definitions()
    pub fn parse_general_definitions(&self) -> Result<GeneralDefinitions<'a>, Error> {
        let data = self.find_raw_section(2)?.ok_or(Error::InvalidBlock)?;
        bytes(data, 0, 5)?;
        let child_size = usize::from(data[4]);
        if !child_device_size_valid(self.version, child_size) {
            return Err(Error::InvalidBlock);
        }
        let child_count = (data.len() - 5) / child_size;
        let children_end = 5 + child_count * child_size;
        Ok(GeneralDefinitions {
            crt_ddc_pin: data[0],
            dpms_non_acpi: data[1] & 1 != 0,
            skip_boot_crt_detect: data[1] & 2 != 0,
            dpms_aim: data[1] & 4 != 0,
            boot_display: [data[2], data[3]],
            record_size: child_size,
            record_size_expected: child_device_expected_size(self.version)
                .is_some_and(|expected| child_size == expected),
            version: self.version,
            children: &data[5..children_end],
        })
    }
    /// Parse the panel-specific eDP settings from BDB block 27.
    // upstream: intel_bios.c parse_edp()
    pub fn parse_edp(
        &self,
        panel_type: u8,
    ) -> Result<Option<EdpConfig>, Error> {
        if panel_type >= 16 {
            return Err(Error::InvalidBlock);
        }
        let Some(edp) = self.find_raw_section(27)? else {
            return Ok(None);
        };
        let index = usize::from(panel_type);
        let power = index * 10;
        let (rate_khz, lanes, preemphasis, vswing) = if self.version >= 224 {
            let rate = u32::from(u16_zero_padded(edp, 748 + index * 2)) * 20;
            let legacy = [
                edp.get(164 + index * 2).copied().unwrap_or(0),
                edp.get(165 + index * 2).copied().unwrap_or(0),
            ];
            (
                rate,
                decode_edp_lanes(legacy[0] >> 4),
                Some(legacy[1] & 0x0f),
                Some(legacy[1] >> 4),
            )
        } else {
            let legacy = [
                edp.get(164 + index * 2).copied().unwrap_or(0),
                edp.get(165 + index * 2).copied().unwrap_or(0),
            ];
            (
                decode_edp_rate(legacy[0] & 0x0f),
                decode_edp_lanes(legacy[0] >> 4),
                Some(legacy[1] & 0x0f),
                Some(legacy[1] >> 4),
            )
        };
        let color_depth = (u32_zero_padded(edp, 160) >> (usize::from(panel_type) * 2)) & 3;
        let sdrs_msa_delay = (u32_zero_padded(edp, 196) >> (usize::from(panel_type) * 2)) & 3;
        let low_vswing = if self.version >= 173 {
            let swing = u64_zero_padded(edp, 204);
            (swing >> (u32::from(panel_type) * 4)) & 0xf == 0
        } else {
            false
        };
        let max_link_rate_khz = if self.version >= 244 {
            Some(u32::from(u16_zero_padded(edp, 780 + index * 2)) * 20)
        } else {
            None
        };
        let dsc_disabled = if self.version >= 251 {
            Some((u16_zero_padded(edp, 812) >> panel_type) & 1 != 0)
        } else {
            None
        };
        let pipe_joiner_enabled = if self.version >= 261 {
            Some((u16_zero_padded(edp, 848) >> panel_type) & 1 != 0)
        } else {
            None
        };
        Ok(Some(EdpConfig {
            panel_type,
            bits_per_pixel: match color_depth {
                0 => Some(18),
                1 => Some(24),
                2 => Some(30),
                _ => None,
            },
            pps: PpsDelays {
                power_up: u16_zero_padded(edp, power),
                backlight_on: u16_zero_padded(edp, power + 2),
                backlight_off: u16_zero_padded(edp, power + 4),
                power_down: u16_zero_padded(edp, power + 6),
                power_cycle: u16_zero_padded(edp, power + 8),
            },
            rate_khz,
            lanes,
            preemphasis,
            vswing,
            low_vswing,
            drrs_msa_timing_delay: sdrs_msa_delay as u8,
            max_link_rate_khz,
            dsc_disabled,
            pipe_joiner_enabled,
        }))
    }
    /// Parse per-panel PSR timing and wake-up policy from BDB block 9.
    // upstream: intel_bios.c parse_psr()
    pub fn parse_psr(
        &self,
        panel_type: u8,
        display_version: u8,
    ) -> Result<Option<PsrConfig>, Error> {
        if panel_type >= 16 {
            return Err(Error::InvalidBlock);
        }
        let Some(psr) = self.find_raw_section(9)? else {
            return Ok(None);
        };
        let table = usize::from(panel_type) * 6;
        let features = psr.get(table).copied().unwrap_or(0);
        let waits = psr.get(table + 1).copied().unwrap_or(0);
        let tp1 = u16_zero_padded(psr, table + 2);
        let tp2_tp3 = u16_zero_padded(psr, table + 4);
        let modern_wakeup = self.version >= 205 && display_version >= 9;
        let tp1_wakeup_us = if modern_wakeup {
            psr_wakeup_time(tp1 as u8, false)
        } else {
            u32::from(tp1) * 100
        };
        let tp2_tp3_wakeup_us = if modern_wakeup {
            psr_wakeup_time(tp2_tp3 as u8, false)
        } else {
            u32::from(tp2_tp3) * 100
        };
        let psr2_tp2_tp3_wakeup_us = if self.version >= 226 {
            let all_panel = u32_zero_padded(psr, 96);
            let code = ((all_panel >> (u32::from(panel_type) * 2)) & 3) as u8;
            psr_wakeup_time(code, true)
        } else {
            tp2_tp3_wakeup_us
        };
        Ok(Some(PsrConfig {
            full_link: features & 1 != 0,
            require_aux_wakeup: features & 2 != 0,
            idle_frames: waits & 0x0f,
            lines_to_wait: (waits >> 4) & 7,
            tp1_wakeup_us,
            tp2_tp3_wakeup_us,
            psr2_tp2_tp3_wakeup_us,
        }))
    }
    /// Resolve the BDB LFP pointers, synthesizing the modern missing block.
    pub fn parse_lfp_data_pointers(&self) -> Result<Option<LfpDataPointers>, Error> {
        let mut pointers = if let Some(raw) = self.find_raw_section(41)? {
            parse_lfp_data_ptrs(raw)
        } else if let Some(generated) = generate_lfp_data_ptrs(self)? {
            generated
        } else {
            return Ok(None);
        };
        let data_offset = raw_block_offset(self.bdb, self.header_size, 42)?;
        if data_offset == 0 {
            return Ok(None);
        }
        let Some(data) = self.find_raw_section(42)? else {
            return Ok(None);
        };
        if !fixup_lfp_data_ptrs(data_offset, data, &mut pointers) {
            return Ok(None);
        }
        Ok(Some(pointers))
    }
    /// Minimum BDB LFP data payload implied by validated pointer tables.
    // upstream: intel_bios.c lfp_data_min_size()
    pub fn lfp_data_min_size(&self) -> Result<usize, Error> {
        let Some(pointers) = self.parse_lfp_data_pointers()? else {
            return Ok(0);
        };
        let mut size = 16 * (46 + 18 + 12);
        if pointers.panel_name.table_size != 0 {
            size = size.max(pointers.panel_name.offset + 310);
        }
        Ok(size)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PpsDelays {
    pub power_up: u16,
    pub backlight_on: u16,
    pub backlight_off: u16,
    pub power_down: u16,
    pub power_cycle: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EdpConfig {
    pub panel_type: u8,
    pub bits_per_pixel: Option<u8>,
    pub pps: PpsDelays,
    pub rate_khz: u32,
    pub lanes: Option<u8>,
    pub preemphasis: Option<u8>,
    pub vswing: Option<u8>,
    pub low_vswing: bool,
    pub drrs_msa_timing_delay: u8,
    pub max_link_rate_khz: Option<u32>,
    pub dsc_disabled: Option<bool>,
    pub pipe_joiner_enabled: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PsrConfig {
    pub full_link: bool,
    pub require_aux_wakeup: bool,
    pub idle_frames: u8,
    pub lines_to_wait: u8,
    pub tp1_wakeup_us: u32,
    pub tp2_tp3_wakeup_us: u32,
    pub psr2_tp2_tp3_wakeup_us: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LfpDataPointer {
    pub offset: usize,
    pub table_size: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LfpDataEntryPointers {
    pub fp_timing: LfpDataPointer,
    pub dvo_timing: LfpDataPointer,
    pub panel_pnp_id: LfpDataPointer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LfpDataPointers {
    pub num_entries: u8,
    pub entries: [LfpDataEntryPointers; 16],
    pub panel_name: LfpDataPointer,
}

/// Read a panel's DVO EDID detailed timing from block 42.
// upstream: intel_bios.c get_lfp_dvo_timing()
pub fn get_lfp_dvo_timing<'a>(
    data: &'a [u8],
    pointers: &LfpDataPointers,
    panel_type: u8,
) -> Option<&'a [u8]> {
    let pointer = pointers.entries.get(usize::from(panel_type))?.dvo_timing;
    data.get(pointer.offset..pointer.offset.checked_add(pointer.table_size)?)
}

/// Read a panel's fixed-panel timing record from block 42.
// upstream: intel_bios.c get_lfp_fp_timing()
pub fn get_lfp_fp_timing<'a>(
    data: &'a [u8],
    pointers: &LfpDataPointers,
    panel_type: u8,
) -> Option<&'a [u8]> {
    let pointer = pointers.entries.get(usize::from(panel_type))?.fp_timing;
    data.get(pointer.offset..pointer.offset.checked_add(pointer.table_size)?)
}

/// Read a panel's PnP ID record from block 42.
// upstream: intel_bios.c get_lfp_pnp_id()
pub fn get_lfp_pnp_id<'a>(
    data: &'a [u8],
    pointers: &LfpDataPointers,
    panel_type: u8,
) -> Option<&'a [u8]> {
    let pointer = pointers.entries.get(usize::from(panel_type))?.panel_pnp_id;
    data.get(pointer.offset..pointer.offset.checked_add(pointer.table_size)?)
}

/// Return the optional panel-name and additional LFP data tail.
// upstream: intel_bios.c get_lfp_data_tail()
pub fn get_lfp_data_tail<'a>(
    data: &'a [u8],
    pointers: &LfpDataPointers,
) -> Option<&'a [u8]> {
    if pointers.panel_name.table_size == 0 {
        return None;
    }
    data.get(pointers.panel_name.offset..)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvoTiming {
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub clock_khz: u32,
    pub width_mm: u16,
    pub height_mm: u16,
    pub hsync_positive: bool,
    pub vsync_positive: bool,
}

/// Convert an EDID detailed timing from the LFP data table to mode timings.
// upstream: intel_bios.c fill_detail_timing_data()
pub fn fill_detail_timing_data(data: &[u8]) -> Result<DvoTiming, Error> {
    bytes(data, 0, 18)?;
    let hdisplay = u16::from(data[2]) | (u16::from(data[4] & 0xf0) << 4);
    let hblank = u16::from(data[3]) | (u16::from(data[4] & 0x0f) << 8);
    let vdisplay = u16::from(data[5]) | (u16::from(data[7] & 0xf0) << 4);
    let vblank = u16::from(data[6]) | (u16::from(data[7] & 0x0f) << 8);
    let hsync_offset = u16::from(data[8]) | (u16::from(data[11] >> 6) << 8);
    let hsync_pulse = u16::from(data[9]) | (u16::from((data[11] >> 4) & 3) << 8);
    let vsync_offset = u16::from(data[10] & 0x0f) | (u16::from((data[11] >> 2) & 3) << 4);
    let vsync_pulse = u16::from(data[10] >> 4) | (u16::from(data[11] & 3) << 4);
    let htotal = hdisplay + hblank;
    let vtotal = vdisplay + vblank;
    let hsync_start = hdisplay + hsync_offset;
    let hsync_end = hsync_start.saturating_add(hsync_pulse).min(htotal);
    let vsync_start = vdisplay + vsync_offset;
    let vsync_end = vsync_start.saturating_add(vsync_pulse).min(vtotal);
    Ok(DvoTiming {
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        clock_khz: u32::from(le16(data, 0)?) * 10,
        width_mm: u16::from(data[12]) | (u16::from(data[14] & 0xf0) << 4),
        height_mm: u16::from(data[13]) | (u16::from(data[14] & 0x0f) << 8),
        hsync_positive: data[17] & (1 << 6) != 0,
        vsync_positive: data[17] & (1 << 5) != 0,
    })
}

/// Translate the packed BDB block 41 pointer structure.
fn parse_lfp_data_ptrs(raw: &[u8]) -> LfpDataPointers {
    let pointer = |offset: usize| LfpDataPointer {
        offset: usize::from(u16_zero_padded(raw, offset)),
        table_size: raw.get(offset + 2).copied().unwrap_or(0) as usize,
    };
    let mut entries = [LfpDataEntryPointers::default(); 16];
    for (index, entry) in entries.iter_mut().enumerate() {
        let base = 1 + index * 9;
        *entry = LfpDataEntryPointers {
            fp_timing: pointer(base),
            dvo_timing: pointer(base + 3),
            panel_pnp_id: pointer(base + 6),
        };
    }
    LfpDataPointers {
        num_entries: raw.first().copied().unwrap_or(0),
        entries,
        panel_name: pointer(145),
    }
}

/// Relocate BDB-relative pointers and validate the resulting panel tables.
// upstream: intel_bios.c fixup_lfp_data_ptrs()
fn fixup_lfp_data_ptrs(
    data_offset: usize,
    data: &[u8],
    pointers: &mut LfpDataPointers,
) -> bool {
    for entry in &mut pointers.entries {
        let Some(offset) = entry.fp_timing.offset.checked_sub(data_offset) else {
            return false;
        };
        entry.fp_timing.offset = offset;
        let Some(offset) = entry.dvo_timing.offset.checked_sub(data_offset) else {
            return false;
        };
        entry.dvo_timing.offset = offset;
        let Some(offset) = entry.panel_pnp_id.offset.checked_sub(data_offset) else {
            return false;
        };
        entry.panel_pnp_id.offset = offset;
    }
    if pointers.panel_name.table_size != 0 {
        let Some(offset) = pointers.panel_name.offset.checked_sub(data_offset) else {
            return false;
        };
        pointers.panel_name.offset = offset;
    }
    validate_lfp_data_ptrs(data, pointers)
}

/// Validate pointer dimensions, uniform spacing, bounds and FP terminators.
// upstream: intel_bios.c validate_lfp_data_ptrs()
fn validate_lfp_data_ptrs(data: &[u8], pointers: &LfpDataPointers) -> bool {
    let data_block_size = data.len();
    if data_block_size == 0 || pointers.num_entries != 3 {
        return false;
    }
    let fp_timing_size = pointers.entries[0].fp_timing.table_size;
    let dvo_timing_size = pointers.entries[0].dvo_timing.table_size;
    let pnp_size = pointers.entries[0].panel_pnp_id.table_size;
    let panel_name_size = pointers.panel_name.table_size;
    if fp_timing_size < 32 || dvo_timing_size != 18 || pnp_size != 12 {
        return false;
    }
    if panel_name_size != 0 && panel_name_size != 13 {
        return false;
    }
    let Some(lfp_data_size) = pointers.entries[1]
        .fp_timing
        .offset
        .checked_sub(pointers.entries[0].fp_timing.offset)
    else {
        return false;
    };
    if lfp_data_size.saturating_mul(16) > data_block_size {
        return false;
    }
    for index in 1..16 {
        let current = pointers.entries[index];
        let previous = pointers.entries[index - 1];
        if current.fp_timing.table_size != fp_timing_size
            || current.dvo_timing.table_size != dvo_timing_size
            || current.panel_pnp_id.table_size != pnp_size
            || current.fp_timing.offset.checked_sub(previous.fp_timing.offset)
                != Some(lfp_data_size)
            || current.dvo_timing.offset.checked_sub(previous.dvo_timing.offset)
                != Some(lfp_data_size)
            || current.panel_pnp_id.offset.checked_sub(previous.panel_pnp_id.offset)
                != Some(lfp_data_size)
        {
            return false;
        }
    }
    let fp_timing_size = if fp_timing_size + 6 + dvo_timing_size + pnp_size == lfp_data_size {
        fp_timing_size + 6
    } else {
        fp_timing_size
    };
    if fp_timing_size + dvo_timing_size + pnp_size != lfp_data_size {
        return false;
    }
    let first = pointers.entries[0];
    if first.fp_timing.offset.checked_add(fp_timing_size) != Some(first.dvo_timing.offset)
        || first.dvo_timing.offset.checked_add(dvo_timing_size)
            != Some(first.panel_pnp_id.offset)
        || first.panel_pnp_id.offset.checked_add(pnp_size) != Some(lfp_data_size)
    {
        return false;
    }
    for entry in pointers.entries {
        if entry.fp_timing.offset.saturating_add(fp_timing_size) > data_block_size
            || entry.dvo_timing.offset.saturating_add(dvo_timing_size) > data_block_size
            || entry.panel_pnp_id.offset.saturating_add(pnp_size) > data_block_size
        {
            return false;
        }
    }
    if pointers
        .panel_name
        .offset
        .saturating_add(16 * panel_name_size)
        > data_block_size
    {
        return false;
    }
    for entry in pointers.entries {
        let Some(end) = entry.fp_timing.offset.checked_add(fp_timing_size) else {
            return false;
        };
        let terminator = u16_zero_padded(data, end - 2);
        if terminator != 0xffff {
            return false;
        }
    }
    true
}

/// Match i915's make_lfp_data_ptr() tail-first offset construction.
// upstream: intel_bios.c make_lfp_data_ptr()
fn make_lfp_data_ptr(table_size: usize, total_size: usize) -> (LfpDataPointer, usize) {
    if total_size < table_size {
        (LfpDataPointer::default(), total_size)
    } else {
        (
            LfpDataPointer {
                offset: total_size - table_size,
                table_size,
            },
            total_size - table_size,
        )
    }
}

/// Advance one entry while preserving the preceding pointer's table size.
// upstream: intel_bios.c next_lfp_data_ptr()
fn next_lfp_data_ptr(previous: LfpDataPointer, size: usize) -> LfpDataPointer {
    LfpDataPointer {
        offset: previous.offset + size,
        table_size: previous.table_size,
    }
}

/// Synthesize block 41 for a modern VBT that omits its LFP data pointers.
// upstream: intel_bios.c generate_lfp_data_ptrs()
fn generate_lfp_data_ptrs(vbt: &Vbt<'_>) -> Result<Option<LfpDataPointers>, Error> {
    if vbt.version < 155 {
        return Ok(None);
    }
    let Some(data) = vbt.find_raw_section(42)? else {
        return Ok(None);
    };
    let fp_timing_size = 38;
    let dvo_timing_size = 18;
    let pnp_size = 12;
    let stride = fp_timing_size + dvo_timing_size + pnp_size;
    if stride * 16 > data.len() {
        return Ok(None);
    }
    let mut first = LfpDataEntryPointers::default();
    let (panel_pnp_id, size) = make_lfp_data_ptr(pnp_size, stride);
    first.panel_pnp_id = panel_pnp_id;
    let (dvo_timing, size) = make_lfp_data_ptr(dvo_timing_size, size);
    first.dvo_timing = dvo_timing;
    let (fp_timing, size) = make_lfp_data_ptr(fp_timing_size, size);
    first.fp_timing = fp_timing;
    if size != 0 {
        return Ok(None);
    }
    let mut entries = [LfpDataEntryPointers::default(); 16];
    entries[0] = first;
    for index in 1..16 {
        let previous = entries[index - 1];
        entries[index] = LfpDataEntryPointers {
            fp_timing: next_lfp_data_ptr(previous.fp_timing, stride),
            dvo_timing: next_lfp_data_ptr(previous.dvo_timing, stride),
            panel_pnp_id: next_lfp_data_ptr(previous.panel_pnp_id, stride),
        };
    }
    let panel_name_size = 13;
    let mut pointers = LfpDataPointers {
        num_entries: 3,
        entries,
        panel_name: LfpDataPointer::default(),
    };
    if 16 * (stride + panel_name_size) <= data.len() {
        pointers.panel_name = LfpDataPointer {
            offset: stride * 16,
            table_size: panel_name_size,
        };
    }
    let offset = raw_block_offset(vbt.bdb, vbt.header_size, 42)?;
    for entry in &mut pointers.entries {
        entry.fp_timing.offset += offset;
        entry.dvo_timing.offset += offset;
        entry.panel_pnp_id.offset += offset;
    }
    if pointers.panel_name.table_size != 0 {
        pointers.panel_name.offset += offset;
    }
    Ok(Some(pointers))
}

const fn psr_wakeup_time(code: u8, psr2: bool) -> u32 {
    match code {
        0 => 500,
        1 => 100,
        3 if psr2 => 50,
        3 => 0,
        _ => 2500,
    }
}

const fn decode_edp_lanes(lanes: u8) -> Option<u8> {
    match lanes {
        0 => Some(1),
        1 => Some(2),
        3 => Some(4),
        _ => None,
    }
}

const fn decode_edp_rate(rate: u8) -> u32 {
    match rate {
        0 => 162_000,
        1 => 270_000,
        2 => 540_000,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GeneralFeatures {
    pub enable_ssc: bool,
    pub ssc_frequency_khz: u32,
    pub display_clock_mode: bool,
    pub rotate_180: bool,
    pub fdi_rx_polarity_inverted: bool,
    pub int_crt_support: bool,
    pub int_tv_support: bool,
    pub afc_startup_config: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrrsType {
    None,
    Static,
    Seamless,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverFeatures {
    pub int_lvds_support: bool,
    pub drrs_enabled: bool,
    pub psr_enabled: bool,
    pub dmrrs_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelPowerFeatures {
    pub drrs_type: DrrsType,
    pub psr_enabled: bool,
    pub vrr_enabled: bool,
    pub hobl_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelOrientation {
    Unknown,
    RightUp,
    BottomUp,
    LeftUp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MipiPpsData {
    pub panel_on_delay: u16,
    pub backlight_on_delay: u16,
    pub backlight_off_delay: u16,
    pub panel_off_delay: u16,
    pub panel_power_cycle_delay: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MipiConfig {
    pub panel_id: u16,
    pub port: Port,
    pub command_mode: bool,
    pub dual_link: bool,
    pub cabc_supported: bool,
    /// DSI backlight port bitmap, using the display `Port` indices.
    pub backlight_ports: u32,
    /// DSI CABC port bitmap, using the display `Port` indices.
    pub cabc_ports: u32,
    pub orientation: PanelOrientation,
    pub pps: MipiPpsData,
    pub pwm_delays: [u16; 2],
    pub pmic_i2c_bus_number: u8,
    pub raw_config: [u8; 122],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MipiSequences<'a> {
    pub version: u8,
    pub sequence_data: &'a [u8],
    pub sequences: [Option<&'a [u8]>; 12],
}

/// Find the sequence payload for the requested panel ID.
// upstream: intel_bios.c find_panel_sequence_block()
fn find_panel_sequence_block<'a>(
    raw: &'a [u8],
    version: u8,
    panel_id: u16,
) -> Result<Option<&'a [u8]>, Error> {
    let (total, data) = if version >= 3 {
        let total = usize::try_from(le32(raw, 1)?).map_err(|_| Error::Truncated)?;
        (total, bytes(raw, 5, raw.len().saturating_sub(5))?)
    } else {
        (raw.len(), bytes(raw, 1, raw.len().saturating_sub(1))?)
    };
    let header_size = if version >= 3 { 5 } else { 3 };
    let mut index = 0usize;
    for _ in 0..6 {
        if index >= total {
            break;
        }
        let header_end = index.checked_add(header_size).ok_or(Error::Truncated)?;
        if header_end > total {
            return Ok(None);
        }
        let current_id = data[index];
        let current_size = if version >= 3 {
            usize::try_from(le32(data, index + 1)?).map_err(|_| Error::Truncated)?
        } else {
            usize::from(le16(data, index + 1)?)
        };
        let end = header_end.checked_add(current_size).ok_or(Error::Truncated)?;
        if end > total || end > data.len() {
            return Ok(None);
        }
        if u16::from(current_id) == panel_id {
            return Ok(Some(&data[header_end..end]));
        }
        index = end;
    }
    Ok(None)
}

/// Walk one v1/v2 sequence's DSI/GPIO/I2C operations and return its next offset.
// upstream: intel_bios.c goto_next_sequence()
fn goto_next_sequence(data: &[u8], sequence_start: usize) -> Result<usize, Error> {
    let mut index = sequence_start.checked_add(1).ok_or(Error::Truncated)?;
    while index < data.len() {
        let operation = data[index];
        index += 1;
        let operand_size = match operation {
            0 => return Ok(index),
            1 => {
                if index.checked_add(4).is_none_or(|end| end > data.len()) {
                    return Err(Error::Truncated);
                }
                usize::from(le16(data, index + 2)?) + 4
            }
            2 => 4,
            3 => 2,
            4 => {
                if index.checked_add(7).is_none_or(|end| end > data.len()) {
                    return Err(Error::Truncated);
                }
                usize::from(data[index + 6]) + 7
            }
            _ => return Err(Error::InvalidBlock),
        };
        index = index.checked_add(operand_size).ok_or(Error::Truncated)?;
        if index > data.len() {
            return Err(Error::Truncated);
        }
    }
    Err(Error::InvalidBlock)
}

/// Walk one v3 sequence using its explicit size and element-length bytes.
// upstream: intel_bios.c goto_next_sequence_v3()
fn goto_next_sequence_v3(data: &[u8], sequence_start: usize) -> Result<usize, Error> {
    if data.len() < 5 {
        return Err(Error::Truncated);
    }
    let mut index = sequence_start.checked_add(1).ok_or(Error::Truncated)?;
    let size = usize::try_from(le32(data, index)?).map_err(|_| Error::Truncated)?;
    index += 4;
    let sequence_end = index.checked_add(size).ok_or(Error::Truncated)?;
    if sequence_end > data.len() {
        return Err(Error::Truncated);
    }
    while index < data.len() {
        let operation = data[index];
        index += 1;
        if operation == 0 {
            return if index == sequence_end {
                Ok(index)
            } else {
                Err(Error::InvalidBlock)
            };
        }
        let operand_size = usize::from(*data.get(index).ok_or(Error::Truncated)?);
        index += 1;
        index = index.checked_add(operand_size).ok_or(Error::Truncated)?;
        if index > data.len() {
            return Err(Error::Truncated);
        }
    }
    Err(Error::InvalidBlock)
}

/// Convert the VBT alternate-SSC bit to the display-version frequency.
// upstream: intel_bios.c intel_bios_ssc_frequency()
pub const fn intel_bios_ssc_frequency(alternate: bool, display_version: u8) -> u32 {
    match display_version {
        2 => if alternate { 66_667 } else { 48_000 },
        3 | 4 => if alternate { 100_000 } else { 96_000 },
        _ => if alternate { 100_000 } else { 120_000 },
    }
}

/// Return the expected child-device record size for a VBT version.
// upstream: intel_bios.c child_device_expected_size()
pub fn child_device_expected_size(version: u16) -> Option<usize> {
    if version > 264 {
        None
    } else if version >= 263 {
        Some(44)
    } else if version >= 256 {
        Some(40)
    } else if version >= 216 {
        Some(39)
    } else if version >= 196 {
        Some(38)
    } else if version >= 195 {
        Some(37)
    } else if version >= 111 {
        Some(33)
    } else if version >= 106 {
        Some(27)
    } else {
        Some(22)
    }
}

/// Whether the child-device stride is large enough to contain the legacy fields.
// upstream: intel_bios.c child_device_size_valid()
pub fn child_device_size_valid(version: u16, size: usize) -> bool {
    let _expected_size = child_device_expected_size(version).unwrap_or(44);
    size >= 33
}

pub struct GeneralDefinitions<'a> {
    pub crt_ddc_pin: u8,
    pub dpms_non_acpi: bool,
    pub skip_boot_crt_detect: bool,
    pub dpms_aim: bool,
    pub boot_display: [u8; 2],
    pub record_size: usize,
    pub record_size_expected: bool,
    version: u16,
    children: &'a [u8],
}
impl<'a> GeneralDefinitions<'a> {
    /// Empty child records are omitted, in upstream list order. Unknown ports
    /// remain visible as None; parsing is not a claim that the PHY is present.
    pub fn children(&self) -> impl Iterator<Item = ChildDevice<'a>> + '_ {
        self.children
            .chunks_exact(self.record_size)
            .filter_map(|record| {
                let device_type = u16::from_le_bytes([record[2], record[3]]);
                if device_type == 0 {
                    return None;
                }
                let byte = |index| record.get(index).copied().unwrap_or(0);
                let dedicated = self.version >= 264 && byte(33) & 4 != 0;
                let mut child = ChildDevice {
                    raw_record: record,
                    handle: u16::from_le_bytes([record[0], record[1]]),
                    device_type,
                    dvo_port: byte(16),
                    port: intel_bios_encoder_port(byte(16), 13),
                    i2c_pin: byte(17),
                    target_addr: byte(18),
                    ddc_pin: byte(19),
                    edid_ptr: u16::from_le_bytes([byte(20), byte(21)]),
                    dvo_cfg: byte(22),
                    dvo2_port: byte(23),
                    i2c2_pin: byte(24),
                    target2_addr: byte(25),
                    ddc2_pin: byte(26),
                    dvo_wiring: byte(28),
                    dvo2_wiring: byte(29),
                    extended_type: u16::from_le_bytes([byte(30), byte(31)]),
                    dvo_function: byte(32),
                    dp_gpio_index: if self.version >= 195 { byte(34) } else { 0 },
                    dp_gpio_pin_num: if self.version >= 195 {
                        u16::from_le_bytes([byte(35), byte(36)])
                    } else {
                        0
                    },
                    dp_iboost_level: if self.version >= 196 { byte(37) & 0x0f } else { 0 },
                    hdmi_iboost_level: if self.version >= 196 { byte(37) >> 4 } else { 0 },
                    dp_max_link_rate: if self.version >= 216 { byte(38) & 7 } else { 0 },
                    efp_index: if self.version >= 256 { byte(39) } else { 0 },
                    edp_data_rate_override: if self.version >= 263 {
                        u32::from_le_bytes([byte(40), byte(41), byte(42), byte(43)]) & 0xfff
                    } else {
                        0
                    },
                    aux_channel: if self.version >= 158 { byte(25) } else { 0 },
                    hdmi_level_shift: if self.version >= 158 { byte(7) & 0x1f } else { 0 },
                    hdmi_max_tmds_khz: if self.version >= 204 {
                        hdmi_max_tmds_clock(byte(7) >> 5)
                    } else {
                        0
                    },
                    lane_reversal: self.version >= 184 && byte(23) & 2 != 0,
                    hpd_invert: self.version >= 196 && byte(23) & 16 != 0,
                    use_vbt_vswing: self.version >= 218 && byte(23) & 32 != 0,
                    lspcon: self.version >= 192 && byte(23) & 4 != 0,
                    dp_max_lane_count: if self.version >= 244 {
                        byte(23) >> 6
                    } else {
                        0
                    },
                    usb_type_c: self.version >= 195 && byte(33) & 1 != 0,
                    thunderbolt: self.version >= 209 && byte(33) & 2 != 0,
                    dedicated_external: dedicated,
                    dynamic_port_over_tc: self.version >= 264 && byte(33) & 8 != 0 && !dedicated,
                };
                sanitize_dedicated_external(&mut child);
                Some(child)
            })
    }
    // upstream: intel_bios.c intel_bios_encoder_data_lookup()
    pub fn encoder(&self, port: Port) -> Option<ChildDevice<'a>> {
        self.children().find(|child| child.port == Some(port))
    }
    /// Return the first routed DSI child and its physical display port.
    // upstream: intel_bios.c intel_bios_is_dsi_present()
    pub fn is_dsi_present(&self, display_version: u8) -> Option<Port> {
        self.children()
            .find(|child| child.supports_dsi())
            .and_then(|child| intel_bios_encoder_port(child.dvo_port, display_version))
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildDevice<'a> {
    pub raw_record: &'a [u8],
    pub handle: u16,
    pub device_type: u16,
    pub dvo_port: u8,
    pub port: Option<Port>,
    pub i2c_pin: u8,
    pub target_addr: u8,
    /// Raw BIOS DDC pin, NOT the hardware GMBUS selector.
    pub ddc_pin: u8,
    pub edid_ptr: u16,
    pub dvo_cfg: u8,
    pub dvo2_port: u8,
    pub i2c2_pin: u8,
    pub target2_addr: u8,
    pub ddc2_pin: u8,
    pub dvo_wiring: u8,
    pub dvo2_wiring: u8,
    pub extended_type: u16,
    pub dvo_function: u8,
    pub dp_gpio_index: u8,
    pub dp_gpio_pin_num: u16,
    pub dp_iboost_level: u8,
    pub hdmi_iboost_level: u8,
    pub dp_max_link_rate: u8,
    pub efp_index: u8,
    pub edp_data_rate_override: u32,
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
impl ChildDevice<'_> {
    // upstream: intel_bios_encoder_supports_crt()
    pub const fn supports_crt(self) -> bool {
        self.device_type & 1 != 0
    }
    // upstream: intel_bios_encoder_supports_dvi()
    pub const fn supports_dvi(self) -> bool {
        self.device_type & (1 << 4) != 0
    }
    // upstream: intel_bios_encoder_supports_hdmi()
    pub const fn supports_hdmi(self) -> bool {
        self.supports_dvi() && self.device_type & (1 << 11) == 0
    }
    // upstream: intel_bios_encoder_supports_dp()
    pub const fn supports_dp(self) -> bool {
        self.device_type & (1 << 2) != 0
    }
    // upstream: intel_bios_encoder_supports_edp()
    pub const fn supports_edp(self) -> bool {
        self.supports_dp() && self.device_type & (1 << 12) != 0
    }
    // upstream: intel_bios_encoder_supports_dsi()
    pub const fn supports_dsi(self) -> bool {
        self.device_type & (1 << 10) != 0
    }
    pub const fn gmbus_pin(self) -> Option<u8> {
        map_ddc_pin(self.ddc_pin)
    }
    // upstream: intel_bios.c intel_bios_dp_max_link_rate()
    pub const fn dp_max_link_rate_khz(self, vbt_version: u16) -> u32 {
        if vbt_version < 216 {
            0
        } else if vbt_version >= 230 {
            parse_bdb_230_dp_max_link_rate(self.dp_max_link_rate)
        } else {
            parse_bdb_216_dp_max_link_rate(self.dp_max_link_rate)
        }
    }
    // upstream: intel_bios.c intel_bios_dp_max_lane_count()
    pub const fn dp_max_lane_count(self, vbt_version: u16) -> u8 {
        if vbt_version < 244 {
            0
        } else {
            self.dp_max_lane_count + 1
        }
    }
    // upstream: intel_bios.c intel_bios_encoder_reject_edp_rate()
    pub const fn reject_edp_rate(self, vbt_version: u16, rate_khz: u32) -> bool {
        if vbt_version < 263 || self.edp_data_rate_override == 0x0fff {
            return false;
        }
        self.edp_data_rate_override & edp_rate_override_mask(rate_khz) != 0
    }
}

/// Decode the VBT 230+ DP-rate selector.
// upstream: intel_bios.c parse_bdb_230_dp_max_link_rate()
pub const fn parse_bdb_230_dp_max_link_rate(rate: u8) -> u32 {
    match rate {
        1 => 162_000,
        2 => 270_000,
        3 => 540_000,
        4 => 810_000,
        5 => 1_000_000,
        6 => 1_350_000,
        7 => 2_000_000,
        _ => 0,
    }
}

/// Decode the VBT 216-229 DP-rate selector.
// upstream: intel_bios.c parse_bdb_216_dp_max_link_rate()
pub const fn parse_bdb_216_dp_max_link_rate(rate: u8) -> u32 {
    match rate {
        0 => 810_000,
        1 => 540_000,
        2 => 270_000,
        3 => 162_000,
        _ => 810_000,
    }
}

/// Return the VBT bit for a specified eDP link rate.
// upstream: intel_bios.c edp_rate_override_mask()
pub const fn edp_rate_override_mask(rate_khz: u32) -> u32 {
    match rate_khz {
        162_000 => 1 << 0,
        216_000 => 1 << 1,
        243_000 => 1 << 2,
        270_000 => 1 << 3,
        324_000 => 1 << 4,
        432_000 => 1 << 5,
        540_000 => 1 << 6,
        675_000 => 1 << 7,
        810_000 => 1 << 8,
        1_000_000 => 1 << 9,
        1_350_000 => 1 << 10,
        2_000_000 => 1 << 11,
        _ => 0,
    }
}

/// Clear Type-C-only child flags for ports declared dedicated external.
// upstream: intel_bios.c sanitize_dedicated_external()
pub fn sanitize_dedicated_external(child: &mut ChildDevice<'_>) {
    if !child.dedicated_external {
        return;
    }
    child.usb_type_c = false;
    child.thunderbolt = false;
    child.dynamic_port_over_tc = false;
}

/// Display 13 XELPD mapping, not the old pre-display-13 port-letter mapping.
// upstream: intel_bios.c dvo_port_to_port()
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

/// Map MIPI DVO port letters to the display engine's port numbering.
// upstream: intel_bios.c dsi_dvo_port_to_port()
pub const fn dsi_dvo_port_to_port(dvo: u8, display_version: u8) -> Option<Port> {
    match dvo {
        21 => Some(Port::A),
        23 if display_version >= 11 => Some(Port::B),
        23 => Some(Port::C),
        _ => None,
    }
}

/// Map a child's DVO field, including the display-11+ DSI fallback.
// upstream: intel_bios.c intel_bios_encoder_port()
pub const fn intel_bios_encoder_port(dvo: u8, display_version: u8) -> Option<Port> {
    match dvo_port_to_port(dvo) {
        Some(port) => Some(port),
        None if display_version >= 11 => dsi_dvo_port_to_port(dvo, display_version),
        None => None,
    }
}

/// Derive DSI backlight and CABC port selectors from the selected config.
// upstream: intel_bios.c parse_dsi_backlight_ports()
fn parse_dsi_backlight_ports(
    vbt_version: u16,
    display_version: u8,
    port_desc: u16,
    port: Port,
    config: &mut MipiConfig,
) {
    if !config.dual_link || vbt_version < 197 {
        let port_bit = 1u32 << port_index(port);
        config.backlight_ports = port_bit;
        config.cabc_ports = if config.cabc_supported { port_bit } else { 0 };
        return;
    }
    let port_a = 1u32 << port_index(Port::A);
    let port_bc = 1u32 << port_index(if display_version >= 11 {
        Port::B
    } else {
        Port::C
    });
    let select = |value| match value {
        0 => port_a,
        1 => port_bc,
        2 => port_a | port_bc,
        _ => port_a | port_bc,
    };
    config.backlight_ports = select((port_desc >> 10) & 3);
    config.cabc_ports = if config.cabc_supported {
        select((port_desc >> 8) & 3)
    } else {
        0
    };
}

const fn port_index(port: Port) -> u8 {
    match port {
        Port::A => 0,
        Port::B => 1,
        Port::C => 2,
        Port::D => 3,
        Port::E => 4,
        Port::Tc1 => 5,
        Port::Tc2 => 6,
        Port::Tc3 => 7,
        Port::Tc4 => 8,
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
// upstream: intel_bios.c intel_bios_hdmi_max_tmds_clock()
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
    fn with_afc_feature(mut data: Vec<u8>, value: u8) -> Vec<u8> {
        let old_sections = data[70..].to_vec();
        let mut feature = vec![0; 8];
        feature[6] = value;
        let mut section = vec![1, 8, 0];
        section.extend_from_slice(&feature);
        section.extend_from_slice(&old_sections);
        data.splice(70.., section);
        let len = data.len();
        data[24..26].copy_from_slice(&(len as u16).to_le_bytes());
        data[68..70].copy_from_slice(&((len - 48) as u16).to_le_bytes());
        data
    }
    fn append_section(mut data: Vec<u8>, id: u8, payload: &[u8]) -> Vec<u8> {
        data.push(id);
        data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        data.extend_from_slice(payload);
        let len = data.len();
        data[24..26].copy_from_slice(&(len as u16).to_le_bytes());
        data[68..70].copy_from_slice(&((len - 48) as u16).to_le_bytes());
        data
    }
    fn append_mipi_sequence_v3(mut data: Vec<u8>, block: &[u8]) -> Vec<u8> {
        data.extend_from_slice(&[53, 0, 0, 3]);
        data.extend_from_slice(&(block.len() as u32).to_le_bytes());
        data.extend_from_slice(block);
        let len = data.len();
        data[24..26].copy_from_slice(&(len as u16).to_le_bytes());
        data[68..70].copy_from_slice(&((len - 48) as u16).to_le_bytes());
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
        let tc = defs.encoder(Port::Tc1).unwrap();
        assert_eq!(tc.gmbus_pin(), Some(9));
        assert!(tc.supports_hdmi());
        assert!(!tc.usb_type_c && !tc.thunderbolt && !tc.supports_dp());
        assert_eq!(tc.hdmi_level_shift, 5);
        assert!(defs.encoder(Port::A).is_none());
    }
    #[test]
    fn general_features_afc_startup_matches_display13_vbt_policy() {
        for (field, expected) in [(0, None), (1, Some(0)), (2, Some(7)), (3, Some(7))] {
            let data = with_afc_feature(table(&tc_hdmi(), 39), field);
            assert_eq!(
                Vbt::parse(&data).unwrap().afc_startup_override(),
                Ok(expected)
            );
        }
        let data = with_afc_feature(table(&tc_hdmi(), 39), 1);
        let mut old = data.clone();
        old[64..66].copy_from_slice(&248u16.to_le_bytes());
        assert_eq!(Vbt::parse(&old).unwrap().afc_startup_override(), Ok(None));
    }
    #[test]
    fn dp_rate_tables_and_edp_override_follow_versioned_vbt_selectors() {
        assert_eq!(parse_bdb_216_dp_max_link_rate(0), 810_000);
        assert_eq!(parse_bdb_216_dp_max_link_rate(3), 162_000);
        assert_eq!(parse_bdb_230_dp_max_link_rate(0), 0);
        assert_eq!(parse_bdb_230_dp_max_link_rate(7), 2_000_000);
        assert_eq!(edp_rate_override_mask(162_000), 1);
        assert_eq!(edp_rate_override_mask(2_000_000), 1 << 11);
        assert_eq!(edp_rate_override_mask(123_000), 0);
    }
    #[test]
    fn edp_parser_reads_panel_slices_and_versioned_tail_fields() {
        let mut edp = vec![0u8; 850];
        edp[0..10].copy_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0]);
        edp[160..164].copy_from_slice(&2u32.to_le_bytes());
        edp[164..166].copy_from_slice(&[0x31, 0x21]);
        edp[196..200].copy_from_slice(&2u32.to_le_bytes());
        edp[204..212].copy_from_slice(&0u64.to_le_bytes());
        edp[748..750].copy_from_slice(&27_000u16.to_le_bytes());
        edp[780..782].copy_from_slice(&27_000u16.to_le_bytes());
        edp[812..814].copy_from_slice(&1u16.to_le_bytes());
        edp[848..850].copy_from_slice(&1u16.to_le_bytes());
        let mut data = append_section(table(&tc_hdmi(), 39), 27, &edp);
        data[64..66].copy_from_slice(&264u16.to_le_bytes());
        let parsed = Vbt::parse(&data).unwrap().parse_edp(0).unwrap().unwrap();
        assert_eq!(parsed.bits_per_pixel, Some(30));
        assert_eq!(parsed.pps.power_cycle, 5);
        assert_eq!(parsed.rate_khz, 540_000);
        assert_eq!(parsed.lanes, Some(4));
        assert_eq!(parsed.preemphasis, Some(1));
        assert_eq!(parsed.vswing, Some(2));
        assert!(parsed.low_vswing && parsed.dsc_disabled.unwrap());
        assert_eq!(parsed.drrs_msa_timing_delay, 2);
        assert_eq!(parsed.max_link_rate_khz, Some(540_000));
        assert_eq!(parsed.pipe_joiner_enabled, Some(true));
    }
    #[test]
    fn psr_parser_selects_panel_fields_and_wakeup_version_encoding() {
        let mut psr = vec![0u8; 100];
        psr[6] = 3;
        psr[7] = 0x57;
        psr[8..10].copy_from_slice(&1u16.to_le_bytes());
        psr[10..12].copy_from_slice(&3u16.to_le_bytes());
        psr[96..100].copy_from_slice(&((3u32) << 2).to_le_bytes());
        let mut data = append_section(table(&tc_hdmi(), 39), 9, &psr);
        data[64..66].copy_from_slice(&226u16.to_le_bytes());
        let parsed = Vbt::parse(&data).unwrap().parse_psr(1, 13).unwrap().unwrap();
        assert!(parsed.full_link && parsed.require_aux_wakeup);
        assert_eq!((parsed.idle_frames, parsed.lines_to_wait), (7, 5));
        assert_eq!(parsed.tp1_wakeup_us, 100);
        assert_eq!(parsed.tp2_tp3_wakeup_us, 0);
        assert_eq!(parsed.psr2_tp2_tp3_wakeup_us, 50);
    }
    #[test]
    fn driver_and_power_conservation_features_apply_versioned_panel_policy() {
        let mut driver = vec![0u8; 19];
        driver[7..9].copy_from_slice(&(1u16 << 10).to_le_bytes());
        driver[17..19].copy_from_slice(&((1u16 << 5) | (1u16 << 9) | (1u16 << 12)).to_le_bytes());
        let mut legacy = append_section(table(&tc_hdmi(), 39), 12, &driver);
        legacy[64..66].copy_from_slice(&227u16.to_le_bytes());
        let vbt = Vbt::parse(&legacy).unwrap();
        let parsed = vbt.parse_driver_features(13).unwrap().unwrap();
        assert!(parsed.int_lvds_support && parsed.drrs_enabled && parsed.psr_enabled);
        assert!(parsed.dmrrs_enabled);
        let panel = vbt
            .parse_panel_driver_features(DrrsType::Seamless, false)
            .unwrap();
        assert_eq!(panel.drrs_type, DrrsType::Seamless);
        assert!(panel.psr_enabled && panel.vrr_enabled);

        let mut power = vec![0u8; 136];
        power[24..26].copy_from_slice(&(1u16 << 2).to_le_bytes());
        power[26..28].copy_from_slice(&0u16.to_le_bytes());
        power[32..34].copy_from_slice(&(1u16 << 2).to_le_bytes());
        power[54..56].copy_from_slice(&(1u16 << 2).to_le_bytes());
        power[56..58].copy_from_slice(&0u16.to_le_bytes());
        let mut modern = append_section(table(&tc_hdmi(), 39), 44, &power);
        modern[64..66].copy_from_slice(&233u16.to_le_bytes());
        let panel = Vbt::parse(&modern)
            .unwrap()
            .parse_power_conservation_features(
                2,
                PanelPowerFeatures {
                    drrs_type: DrrsType::Seamless,
                    psr_enabled: false,
                    vrr_enabled: false,
                    hobl_enabled: false,
                },
            )
            .unwrap();
        assert_eq!(panel.drrs_type, DrrsType::Static);
        assert!(panel.psr_enabled && panel.hobl_enabled);
        assert!(!panel.vrr_enabled);
    }
    #[test]
    fn mipi_config_uses_child_port_and_panel_indexed_pps() {
        let mut child = [0u8; 39];
        child[2..4].copy_from_slice(&(1u16 << 10).to_le_bytes());
        child[16] = 21;
        let mut mipi = vec![0u8; 822];
        mipi[0..2].copy_from_slice(&7u16.to_le_bytes());
        mipi[2..6].copy_from_slice(&((1u32 << 5) | (1u32 << 8) | (2u32 << 14)).to_le_bytes());
        mipi[6..8].copy_from_slice(&(1u16 | (1 << 8) | (2 << 10)).to_le_bytes());
        mipi[732..742].copy_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0]);
        mipi[792..796].copy_from_slice(&[6, 0, 7, 0]);
        mipi[816] = 9;
        let data = append_section(table(&child, 39), 52, &mipi);
        let parsed = Vbt::parse(&data)
            .unwrap()
            .parse_mipi_config(0, 13)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.panel_id, 1);
        assert_eq!(parsed.port, Port::A);
        assert!(parsed.command_mode && parsed.dual_link && parsed.cabc_supported);
        assert_eq!(parsed.backlight_ports, 3); // A + B on display 13
        assert_eq!(parsed.cabc_ports, 2); // B only
        assert_eq!(parsed.orientation, PanelOrientation::BottomUp);
        assert_eq!(parsed.pps.panel_power_cycle_delay, 5);
        assert_eq!(parsed.pwm_delays, [6, 7]);
        assert_eq!(parsed.pmic_i2c_bus_number, 9);
    }
    #[test]
    fn mipi_v3_sequence_walker_indexes_steps_and_applies_display11_fixup() {
        let mut child = [0u8; 39];
        child[2..4].copy_from_slice(&(1u16 << 10).to_le_bytes());
        child[16] = 21;
        let mipi = vec![0u8; 822];
        let mut data = append_section(table(&child, 39), 52, &mipi);
        // One panel record (id 0, length 7): DISPLAY_ON, v3 size 1,
        // element END, then the MIPI sequence END sentinel.
        let sequence_block = [0, 7, 0, 0, 0, 3, 1, 0, 0, 0, 0, 0];
        data = append_mipi_sequence_v3(data, &sequence_block);
        let parsed = Vbt::parse(&data)
            .unwrap()
            .parse_mipi_sequence(0, 13)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.version, 3);
        assert!(parsed.sequences[2].is_some()); // moved DISPLAY_ON to INIT_OTP
        assert!(parsed.sequences[3].is_none());
    }
    #[test]
    fn lfp_pointer_generation_uses_upstream_stride_and_validates_terminators() {
        let stride = 38 + 18 + 12;
        let mut data_block = vec![0u8; stride * 16];
        for panel in 0..16 {
            let term = panel * stride + 36;
            data_block[term..term + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        }
        let data = append_section(table(&tc_hdmi(), 39), 42, &data_block);
        let vbt = Vbt::parse(&data).unwrap();
        let generated = vbt.parse_lfp_data_pointers().unwrap().unwrap();
        assert_eq!(generated.num_entries, 3);
        assert_eq!(generated.entries[0].fp_timing.offset, 0);
        assert_eq!(generated.entries[0].dvo_timing.offset, 38);
        assert_eq!(generated.entries[0].panel_pnp_id.offset, 56);
        assert_eq!(generated.entries[15].fp_timing.offset, stride * 15);
        assert_eq!(generated.panel_name.table_size, 0);
        assert_eq!(vbt.lfp_data_min_size().unwrap(), 1216);

        let mut bad = data_block;
        bad[36..38].fill(0);
        let bad_data = append_section(table(&tc_hdmi(), 39), 42, &bad);
        assert!(Vbt::parse(&bad_data)
            .unwrap()
            .parse_lfp_data_pointers()
            .unwrap()
            .is_none());
    }
    #[test]
    fn lfp_dvo_detail_timing_unpacks_and_clamps_edid_dtd_fields() {
        let mut dtd = [0u8; 18];
        dtd[0..2].copy_from_slice(&14_850u16.to_le_bytes());
        dtd[2] = 0x80;
        dtd[3] = 0x18;
        dtd[4] = 0x71;
        dtd[5] = 0x38;
        dtd[6] = 45;
        dtd[7] = 0x40;
        dtd[8] = 88;
        dtd[9] = 44;
        dtd[10] = 0x54;
        dtd[12] = 0x12;
        dtd[13] = 0x34;
        dtd[14] = 0x56;
        dtd[17] = 0x60;
        let timing = fill_detail_timing_data(&dtd).unwrap();
        assert_eq!((timing.hdisplay, timing.htotal), (1920, 2200));
        assert_eq!((timing.vdisplay, timing.vtotal), (1080, 1125));
        assert_eq!((timing.hsync_start, timing.hsync_end), (2008, 2052));
        assert_eq!((timing.vsync_start, timing.vsync_end), (1084, 1089));
        assert_eq!(timing.clock_khz, 148_500);
        assert_eq!((timing.width_mm, timing.height_mm), (0x512, 0x634));
        assert!(timing.hsync_positive && timing.vsync_positive);
    }
    #[test]
    fn every_truncation_and_bad_extent_fails_without_panic() {
        let data = table(&tc_hdmi(), 39);
        for end in 0..data.len() {
            assert!(Vbt::parse(&data[..end]).is_err(), "{end}");
        }
        for offset in [24, 28, 68] {
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
        assert_eq!(vbt.find_raw_section(250).unwrap(), None);
    }
    #[test]
    fn zero_stride_is_rejected_but_tail_and_duplicate_records_follow_i915_order() {
        assert!(
            Vbt::parse(&table(&tc_hdmi(), 0))
                .unwrap()
                .parse_general_definitions()
                .is_err()
        );
        let partial_data = table(&tc_hdmi()[..38], 39);
        let partial = Vbt::parse(&partial_data)
            .unwrap()
            .parse_general_definitions()
            .unwrap();
        assert_eq!(partial.children().count(), 0);
        let mut both = tc_hdmi().to_vec();
        both.extend_from_slice(&tc_hdmi());
        let data = table(&both, 39);
        let vbt = Vbt::parse(&data).unwrap();
        assert_eq!(
            vbt.parse_general_definitions()
                .unwrap()
                .encoder(Port::Tc1)
                .unwrap()
                .handle,
            64
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
            .unwrap();
        assert!(tc.dedicated_external);
        assert!(!tc.usb_type_c && !tc.thunderbolt && !tc.dynamic_port_over_tc);
        data[64..66].copy_from_slice(&263u16.to_le_bytes());
        let vbt = Vbt::parse(&data).unwrap();
        let tc = vbt
            .parse_general_definitions()
            .unwrap()
            .encoder(Port::Tc1)
            .unwrap();
        assert!(!tc.dedicated_external);
        assert!(tc.usb_type_c && tc.thunderbolt);
    }
    #[test]
    fn mipi_v3_size_field_is_not_the_short_block_size() {
        let mut data = table(&tc_hdmi(), 39);
        data[70] = 53;
        data[73] = 3;
        let size = (data.len() - 78) as u32;
        data[74..78].copy_from_slice(&size.to_le_bytes());
        let vbt = Vbt::parse(&data).unwrap();
        assert_eq!(
            vbt.find_raw_section(53).unwrap().unwrap().len(),
            size as usize + 5
        );
        data[74..78].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Vbt::parse(&data).unwrap().find_raw_section(53).unwrap(),
            None
        );
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
