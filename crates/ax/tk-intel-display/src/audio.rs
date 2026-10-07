// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_audio.c:
// audio_config_hdmi_pixel_clock, audio_config_hdmi_get_n,
// hsw_hdmi_audio_config_update and the DDI codec enable/disable sequencing
// (display-13 HDMI subset). Selected fields from intel_audio_regs.h.
// DRM ELD byte layout follows the individually MIT-licensed
// drivers/gpu/drm/drm_edid.c::drm_edid_to_eld and include/drm/drm_eld.h.
// Copyright © 2014 Intel Corporation.
// Copyright © 2022 Intel Corporation.
// Copyright © 2023 Intel Corporation.
// Linux drm_edid.c includes Copyright (c) 2006 Luc Verhaegen,
// Copyright (c) 2007-2008 Intel Corporation and Copyright 2010 Red Hat, Inc.
// MIT permission text: ../LICENSE-MIT. Only HDMI pipe-A LPCM 2ch/48kHz,
// fixed 8bpc clocks are admitted; audio output is opt-in with the native link.
use crate::{Error, RegisterIo, display::Pipe};

pub const MAX_ELD_SIZE: usize = 84;
const EDID_BLOCK_SIZE: usize = 128;
const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
const ELD_HEADER_SIZE: usize = 4;
const ELD_BASELINE_FIXED_SIZE: usize = 16;
const ELD_SAD_LIMIT: usize = 15;

/// Why a sink EDID could not produce an HDMI audio ELD.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EldError {
    InvalidEdid,
    Truncated,
    InvalidCta,
    NoAudio,
    UnsupportedPcm,
    TooManySad,
}

/// HDMI ELD bytes and the exact padded baseline size delivered to HDA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Eld {
    bytes: [u8; MAX_ELD_SIZE],
    len: u8,
    sad_count: u8,
}

impl Eld {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    pub const fn sad_count(&self) -> u8 {
        self.sad_count
    }

    /// The bounded HDA path supports only two-channel linear PCM, 48 kHz, 16-bit.
    pub fn supports_stereo_s16le_48khz(&self) -> bool {
        let start = ELD_HEADER_SIZE + ELD_BASELINE_FIXED_SIZE + usize::from(self.bytes[4] & 0x1f);
        let end = start + usize::from(self.sad_count) * 3;
        self.bytes.get(start..end).is_some_and(|bytes| {
            bytes
                .as_chunks::<3>()
                .0
                .iter()
                .any(sad_supports_stereo_s16le_48khz)
        })
    }
}

fn checksum(block: &[u8]) -> bool {
    block.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn sad_supports_stereo_s16le_48khz(sad: &[u8; 3]) -> bool {
    ((sad[0] >> 3) & 0x0f) == 1 && (sad[0] & 0x07) >= 1 && sad[1] & (1 << 2) != 0 && sad[2] & 1 != 0
}

/// Builds the baseline ELD from validated EDID bytes, retaining CTA Audio SADs.
///
/// The caller supplies the exact EDID image used to admit the connected HDMI
/// sink. Every declared block and checksum is checked again here: partial
/// connector reads cannot be promoted into HDA capabilities.
pub fn build_eld(edid: &[u8]) -> Result<Eld, EldError> {
    if edid.len() < EDID_BLOCK_SIZE {
        return Err(EldError::Truncated);
    }
    let base = &edid[..EDID_BLOCK_SIZE];
    if base[..8] != EDID_HEADER || !checksum(base) {
        return Err(EldError::InvalidEdid);
    }
    let extension_count = usize::from(base[126]);
    let block_count = extension_count
        .checked_add(1)
        .ok_or(EldError::InvalidEdid)?;
    let expected_size = block_count
        .checked_mul(EDID_BLOCK_SIZE)
        .ok_or(EldError::InvalidEdid)?;
    if edid.len() != expected_size {
        return Err(EldError::Truncated);
    }

    let mut sads = [[0u8; 3]; ELD_SAD_LIMIT];
    let mut sad_count = 0usize;
    let mut cea_revision = 0u8;
    let mut speaker_allocation = 0u8;
    for block in edid[EDID_BLOCK_SIZE..].as_chunks::<EDID_BLOCK_SIZE>().0 {
        if !checksum(block) {
            return Err(EldError::InvalidEdid);
        }
        if block[0] != 0x02 || block[1] == 0 {
            continue;
        }
        let offset = usize::from(block[2]);
        if offset == 0 {
            continue;
        }
        if !(4..127).contains(&offset) {
            return Err(EldError::InvalidCta);
        }
        cea_revision = cea_revision.max(block[1].min(7));
        let mut cursor = 4usize;
        while cursor < offset {
            let header = block[cursor];
            cursor += 1;
            let tag = header >> 5;
            let payload_len = usize::from(header & 0x1f);
            let end = cursor
                .checked_add(payload_len)
                .filter(|end| *end <= offset)
                .ok_or(EldError::InvalidCta)?;
            let payload = &block[cursor..end];
            match tag {
                1 => {
                    if payload.len() % 3 != 0 {
                        return Err(EldError::InvalidCta);
                    }
                    for sad in payload.as_chunks::<3>().0 {
                        if sad_count == ELD_SAD_LIMIT {
                            break;
                        }
                        sads[sad_count].copy_from_slice(sad);
                        sad_count += 1;
                    }
                }
                4 => {
                    if let Some(&allocation) = payload.first() {
                        speaker_allocation = allocation & 0x7f;
                    }
                }
                _ => {}
            }
            cursor = end;
        }
    }
    if sad_count == 0 {
        return Err(EldError::NoAudio);
    }

    let baseline_bytes = ELD_BASELINE_FIXED_SIZE + sad_count * 3;
    let baseline_dwords = baseline_bytes.div_ceil(4);
    let eld_bytes = ELD_HEADER_SIZE + baseline_dwords * 4;
    if eld_bytes > MAX_ELD_SIZE || baseline_dwords > u8::MAX as usize {
        return Err(EldError::TooManySad);
    }
    let mut bytes = [0u8; MAX_ELD_SIZE];
    bytes[0] = 2 << 3; // ELD version 2 (CEA-861-D baseline).
    bytes[2] = baseline_dwords as u8;
    bytes[4] = (cea_revision << 5) & 0xe0; // MNL is zero: no guessed monitor name.
    bytes[5] = (sad_count as u8) << 4; // HDMI connection, no unobserved protection flags.
    bytes[7] = speaker_allocation;
    // DRM's MIT ELD helper preserves the EDID manufacturer/product bytes.
    bytes[16] = base[8];
    bytes[17] = base[9];
    bytes[18] = base[10];
    bytes[19] = base[11];
    for (index, sad) in sads[..sad_count].iter().enumerate() {
        let start = 20 + index * 3;
        bytes[start..start + 3].copy_from_slice(sad);
    }
    let eld = Eld {
        bytes,
        len: eld_bytes as u8,
        sad_count: sad_count as u8,
    };
    if !eld.supports_stereo_s16le_48khz() {
        return Err(EldError::UnsupportedPcm);
    }
    Ok(eld)
}

/// Display-13 audio power and frame sequencing supplied by the kernel adapter.
/// Implementations must only return success when the pipe and audio power domain
/// are already held; this interface never requests a display power well.
pub trait AudioIo: RegisterIo {
    fn audio_power_held(&self, pipe: Pipe) -> Result<(), Error>;
    fn wait_vblanks(&self, pipe: Pipe, count: u8) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioError {
    UnsupportedPipe,
    UnsupportedClock(u32),
    InvalidSinkEld,
    AlreadyEnabled,
    RestoreFailed,
}

const AUD_PIN_BUF_CTL: u32 = 0x48414;
const AUD_PIN_BUF_ENABLE: u32 = 1 << 31;
const HSW_AUD_CFG_A: u32 = 0x65000;
const HSW_AUD_M_CTS_ENABLE_A: u32 = 0x65028;
const HSW_AUD_PIN_ELD_CP_VLD: u32 = 0x650c0;
const AUDIO_OUTPUT_ENABLE_A: u32 = 1 << 2;
const AUDIO_ELD_VALID_A: u32 = 1;
const AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK: u32 = 0x000f_0000;
const AUD_CONFIG_N_LOWER_MASK: u32 = 0x0000_fff0;
const AUD_CONFIG_N_UPPER_MASK: u32 = 0x0ff0_0000;
const AUD_CONFIG_N_MASK: u32 = AUD_CONFIG_N_LOWER_MASK | AUD_CONFIG_N_UPPER_MASK;
const AUD_CONFIG_N_PROG_ENABLE: u32 = 1 << 28;
const AUD_CONFIG_N_VALUE_INDEX: u32 = 1 << 29;
const AUD_M_CTS_M_VALUE_INDEX: u32 = 1 << 21;
const AUD_M_CTS_M_PROG_ENABLE: u32 = 1 << 20;
const AUD_CONFIG_OWNED_MASK: u32 = AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK
    | AUD_CONFIG_N_MASK
    | AUD_CONFIG_N_PROG_ENABLE
    | AUD_CONFIG_N_VALUE_INDEX;
const AUD_M_CTS_OWNED_MASK: u32 = AUD_M_CTS_M_VALUE_INDEX | AUD_M_CTS_M_PROG_ENABLE;

fn hdmi_pixel_clock_index(clock_khz: u32) -> Option<u32> {
    Some(match clock_khz {
        25_175 => 0,
        25_200 => 1,
        27_000 => 2,
        27_027 => 3,
        54_000 => 4,
        54_054 => 5,
        74_176 => 6,
        74_250 => 7,
        148_352 => 8,
        148_500 => 9,
        296_703 => 10,
        297_000 => 11,
        593_407 => 12,
        594_000 => 13,
        _ => return None,
    })
}

fn hdmi_n_24bpp_48khz(clock_khz: u32) -> u32 {
    match clock_khz {
        296_703 => 5_824,
        297_000 => 5_120,
        593_407 => 5_824,
        594_000 => 6_144,
        _ => 0, // Linux lets hardware calculate N for clocks without a table entry.
    }
}

fn encode_n(n: u32) -> u32 {
    ((n >> 12) << 20) & AUD_CONFIG_N_UPPER_MASK | (n << 4) & AUD_CONFIG_N_LOWER_MASK
}

/// A live, already-powered DDI audio handshake. The caller owns link ordering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Session {
    pipe: Pipe,
    previous_pin_buf: u32,
    previous_config_fields: u32,
    previous_m_cts_fields: u32,
    was_enabled: bool,
}

impl Session {
    /// Enables the HSW/DDI audio presence sequence only after a stable HDMI pipe.
    pub fn enable(
        io: &impl AudioIo,
        pipe: Pipe,
        pixel_clock_khz: u32,
        eld: &Eld,
    ) -> Result<Self, AudioError> {
        if pipe != Pipe::A {
            return Err(AudioError::UnsupportedPipe);
        }
        if !eld.supports_stereo_s16le_48khz() {
            return Err(AudioError::InvalidSinkEld);
        }
        let pixel_clock = hdmi_pixel_clock_index(pixel_clock_khz)
            .ok_or(AudioError::UnsupportedClock(pixel_clock_khz))?;
        io.audio_power_held(pipe)
            .map_err(|_| AudioError::UnsupportedPipe)?;
        let previous_pin_buf = io
            .read32(AUD_PIN_BUF_CTL)
            .map_err(|_| AudioError::UnsupportedPipe)?;
        let previous_cfg = io
            .read32(HSW_AUD_CFG_A)
            .map_err(|_| AudioError::UnsupportedPipe)?;
        let previous_m_cts = io
            .read32(HSW_AUD_M_CTS_ENABLE_A)
            .map_err(|_| AudioError::UnsupportedPipe)?;
        let previous_pin = io
            .read32(HSW_AUD_PIN_ELD_CP_VLD)
            .map_err(|_| AudioError::UnsupportedPipe)?;
        if previous_pin & (AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A) != 0 {
            return Err(AudioError::AlreadyEnabled);
        }

        let n = hdmi_n_24bpp_48khz(pixel_clock_khz);
        let mut config = previous_cfg & !AUD_CONFIG_OWNED_MASK;
        config |= pixel_clock << 16;
        if n != 0 {
            config |= AUD_CONFIG_N_PROG_ENABLE | encode_n(n);
        }
        let m_cts = previous_m_cts & !AUD_M_CTS_OWNED_MASK;
        let result = (|| -> Result<(), Error> {
            io.write32(AUD_PIN_BUF_CTL, previous_pin_buf | AUD_PIN_BUF_ENABLE)?;
            if io.read32(AUD_PIN_BUF_CTL)? & AUD_PIN_BUF_ENABLE == 0 {
                return Err(Error::Refused);
            }
            io.write32(HSW_AUD_CFG_A, config)?;
            if io.read32(HSW_AUD_CFG_A)? & AUD_CONFIG_OWNED_MASK != config & AUD_CONFIG_OWNED_MASK {
                return Err(Error::Refused);
            }
            io.write32(HSW_AUD_M_CTS_ENABLE_A, m_cts)?;
            if io.read32(HSW_AUD_M_CTS_ENABLE_A)? & AUD_M_CTS_OWNED_MASK
                != m_cts & AUD_M_CTS_OWNED_MASK
            {
                return Err(Error::Refused);
            }
            io.write32(HSW_AUD_PIN_ELD_CP_VLD, previous_pin | AUDIO_OUTPUT_ENABLE_A)?;
            if io.read32(HSW_AUD_PIN_ELD_CP_VLD)? & AUDIO_OUTPUT_ENABLE_A == 0 {
                return Err(Error::Refused);
            }
            io.wait_vblanks(pipe, 1)?;
            let pin = io.read32(HSW_AUD_PIN_ELD_CP_VLD)?;
            io.write32(HSW_AUD_PIN_ELD_CP_VLD, pin & !AUDIO_ELD_VALID_A)?;
            if io.read32(HSW_AUD_PIN_ELD_CP_VLD)? & AUDIO_ELD_VALID_A != 0 {
                return Err(Error::Refused);
            }
            Ok(())
        })();
        if result.is_err() {
            // Restore only fields this sequence owns; a landed store is possible.
            if restore(
                io,
                pipe,
                previous_pin_buf,
                previous_cfg,
                previous_m_cts,
                previous_pin,
            )
            .is_err()
            {
                return Err(AudioError::RestoreFailed);
            }
            return Err(AudioError::UnsupportedPipe);
        }
        Ok(Self {
            pipe,
            previous_pin_buf,
            previous_config_fields: previous_cfg & AUD_CONFIG_OWNED_MASK,
            previous_m_cts_fields: previous_m_cts & AUD_M_CTS_OWNED_MASK,
            was_enabled: true,
        })
    }

    /// Invalidate ELD, wait two fresh frames, then turn off presence detect.
    /// The caller must run this before disabling the transcoder/port.
    pub fn disable(&mut self, io: &impl AudioIo) -> Result<(), Error> {
        if !self.was_enabled {
            return Ok(());
        }
        io.audio_power_held(self.pipe)?;
        let pin = io.read32(HSW_AUD_PIN_ELD_CP_VLD)?;
        io.write32(HSW_AUD_PIN_ELD_CP_VLD, pin & !AUDIO_ELD_VALID_A)?;
        if io.read32(HSW_AUD_PIN_ELD_CP_VLD)? & AUDIO_ELD_VALID_A != 0 {
            return Err(Error::Refused);
        }
        io.wait_vblanks(self.pipe, 2)?;
        let pin = io.read32(HSW_AUD_PIN_ELD_CP_VLD)?;
        io.write32(
            HSW_AUD_PIN_ELD_CP_VLD,
            pin & !(AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A),
        )?;
        if io.read32(HSW_AUD_PIN_ELD_CP_VLD)? & (AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A) != 0 {
            return Err(Error::Refused);
        }
        let current_config = io.read32(HSW_AUD_CFG_A)?;
        io.write32(
            HSW_AUD_CFG_A,
            (current_config & !AUD_CONFIG_OWNED_MASK) | self.previous_config_fields,
        )?;
        if io.read32(HSW_AUD_CFG_A)? & AUD_CONFIG_OWNED_MASK != self.previous_config_fields {
            return Err(Error::Refused);
        }
        let current_m_cts = io.read32(HSW_AUD_M_CTS_ENABLE_A)?;
        io.write32(
            HSW_AUD_M_CTS_ENABLE_A,
            (current_m_cts & !AUD_M_CTS_OWNED_MASK) | self.previous_m_cts_fields,
        )?;
        if io.read32(HSW_AUD_M_CTS_ENABLE_A)? & AUD_M_CTS_OWNED_MASK != self.previous_m_cts_fields {
            return Err(Error::Refused);
        }
        let current_buf = io.read32(AUD_PIN_BUF_CTL)?;
        io.write32(
            AUD_PIN_BUF_CTL,
            (current_buf & !AUD_PIN_BUF_ENABLE) | (self.previous_pin_buf & AUD_PIN_BUF_ENABLE),
        )?;
        if io.read32(AUD_PIN_BUF_CTL)? & AUD_PIN_BUF_ENABLE
            != self.previous_pin_buf & AUD_PIN_BUF_ENABLE
        {
            return Err(Error::Refused);
        }
        self.was_enabled = false;
        Ok(())
    }
}

fn restore(
    io: &impl AudioIo,
    pipe: Pipe,
    previous_pin_buf: u32,
    previous_config: u32,
    previous_m_cts: u32,
    previous_pin: u32,
) -> Result<(), Error> {
    io.audio_power_held(pipe)?;
    let pin = io.read32(HSW_AUD_PIN_ELD_CP_VLD)?;
    io.write32(HSW_AUD_PIN_ELD_CP_VLD, pin & !AUDIO_ELD_VALID_A)?;
    if pin & AUDIO_OUTPUT_ENABLE_A != 0 {
        io.wait_vblanks(pipe, 2)?;
    }
    let pin = io.read32(HSW_AUD_PIN_ELD_CP_VLD)?;
    io.write32(
        HSW_AUD_PIN_ELD_CP_VLD,
        (pin & !AUDIO_OUTPUT_ENABLE_A) | (previous_pin & AUDIO_OUTPUT_ENABLE_A),
    )?;
    if io.read32(HSW_AUD_PIN_ELD_CP_VLD)? & (AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A)
        != previous_pin & (AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A)
    {
        return Err(Error::RestoreFailed(HSW_AUD_PIN_ELD_CP_VLD));
    }
    let cfg = io.read32(HSW_AUD_CFG_A)?;
    io.write32(
        HSW_AUD_CFG_A,
        (cfg & !AUD_CONFIG_OWNED_MASK) | (previous_config & AUD_CONFIG_OWNED_MASK),
    )?;
    if io.read32(HSW_AUD_CFG_A)? & AUD_CONFIG_OWNED_MASK != previous_config & AUD_CONFIG_OWNED_MASK
    {
        return Err(Error::RestoreFailed(HSW_AUD_CFG_A));
    }
    let m_cts = io.read32(HSW_AUD_M_CTS_ENABLE_A)?;
    io.write32(
        HSW_AUD_M_CTS_ENABLE_A,
        (m_cts & !AUD_M_CTS_OWNED_MASK) | (previous_m_cts & AUD_M_CTS_OWNED_MASK),
    )?;
    if io.read32(HSW_AUD_M_CTS_ENABLE_A)? & AUD_M_CTS_OWNED_MASK
        != previous_m_cts & AUD_M_CTS_OWNED_MASK
    {
        return Err(Error::RestoreFailed(HSW_AUD_M_CTS_ENABLE_A));
    }
    let buf = io.read32(AUD_PIN_BUF_CTL)?;
    io.write32(
        AUD_PIN_BUF_CTL,
        (buf & !AUD_PIN_BUF_ENABLE) | (previous_pin_buf & AUD_PIN_BUF_ENABLE),
    )?;
    if io.read32(AUD_PIN_BUF_CTL)? & AUD_PIN_BUF_ENABLE != previous_pin_buf & AUD_PIN_BUF_ENABLE {
        return Err(Error::RestoreFailed(AUD_PIN_BUF_CTL));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::cell::{Cell, RefCell};
    use std::{collections::BTreeMap, vec, vec::Vec};

    use super::*;

    extern crate std;

    fn edid_with_audio(sads: &[u8]) -> Vec<u8> {
        let mut image = vec![0u8; 256];
        image[..8].copy_from_slice(&EDID_HEADER);
        image[8..12].copy_from_slice(&[0x4c, 0x2d, 0x34, 0x12]);
        image[126] = 1;
        image[127] = checksum_byte(&image[..127]);
        let cta = &mut image[128..];
        cta[0] = 2;
        cta[1] = 3;
        cta[2] = (4 + 1 + sads.len()) as u8;
        cta[3] = 0x40;
        cta[4] = (1 << 5) | sads.len() as u8;
        cta[5..5 + sads.len()].copy_from_slice(sads);
        cta[127] = checksum_byte(&cta[..127]);
        image
    }

    fn checksum_byte(bytes: &[u8]) -> u8 {
        0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
    }

    #[test]
    fn builds_exact_padded_baseline_eld_and_pcm_capability() {
        let image = edid_with_audio(&[0x09, 1 << 2, 1]);
        let eld = build_eld(&image).unwrap();
        assert_eq!(eld.as_bytes().len(), 24);
        assert_eq!(&eld.as_bytes()[..8], &[0x10, 0, 5, 0, 0x60, 0x10, 0, 0]);
        assert_eq!(
            &eld.as_bytes()[16..23],
            &[0x4c, 0x2d, 0x34, 0x12, 0x09, 0x04, 1]
        );
        assert!(eld.supports_stereo_s16le_48khz());
    }

    #[test]
    fn rejects_partial_or_corrupt_edid_and_unsupported_sink_audio() {
        assert_eq!(build_eld(&[0; 128]), Err(EldError::InvalidEdid));
        let mut image = edid_with_audio(&[0x09, 1 << 2, 1]);
        image[200] ^= 1;
        assert_eq!(build_eld(&image), Err(EldError::InvalidEdid));
        assert_eq!(
            build_eld(&edid_with_audio(&[0x09, 1 << 2, 2])),
            Err(EldError::UnsupportedPcm)
        );
        let mut image = edid_with_audio(&[]);
        image[126] = 2;
        image[127] = checksum_byte(&image[..127]);
        assert_eq!(build_eld(&image), Err(EldError::Truncated));
    }

    #[test]
    fn unknown_pixel_clock_is_refused_before_register_mutation() {
        let model = Model::default();
        let initial = model.words.borrow().clone();
        let eld = build_eld(&edid_with_audio(&[0x09, 1 << 2, 1])).unwrap();
        assert_eq!(
            Session::enable(&model, Pipe::A, 40_000, &eld),
            Err(AudioError::UnsupportedClock(40_000))
        );
        assert!(model.writes.borrow().is_empty());
        assert_eq!(*model.words.borrow(), initial);
    }

    struct Model {
        words: RefCell<BTreeMap<u32, u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
        frames: Cell<u8>,
        fail_after: Cell<Option<usize>>,
        wait_calls: Cell<usize>,
        fail_wait_on_call: Cell<Option<usize>>,
    }

    impl Default for Model {
        fn default() -> Self {
            let mut words = BTreeMap::new();
            words.insert(AUD_PIN_BUF_CTL, 0x1234);
            words.insert(HSW_AUD_CFG_A, 0x8000_0001);
            words.insert(HSW_AUD_M_CTS_ENABLE_A, 0x0040_0000);
            words.insert(HSW_AUD_PIN_ELD_CP_VLD, 0x40);
            Self {
                words: RefCell::new(words),
                writes: RefCell::new(Vec::new()),
                frames: Cell::new(0),
                fail_after: Cell::new(None),
                wait_calls: Cell::new(0),
                fail_wait_on_call: Cell::new(None),
            }
        }
    }

    impl RegisterIo for Model {
        fn read32(&self, offset: u32) -> Result<u32, Error> {
            Ok(*self.words.borrow().get(&offset).unwrap_or(&0))
        }
        fn write32(&self, offset: u32, value: u32) -> Result<(), Error> {
            let index = self.writes.borrow().len();
            self.words.borrow_mut().insert(offset, value);
            self.writes.borrow_mut().push((offset, value));
            if self.fail_after.get() == Some(index) {
                Err(Error::Unavailable(offset))
            } else {
                Ok(())
            }
        }
    }

    impl AudioIo for Model {
        fn audio_power_held(&self, _pipe: Pipe) -> Result<(), Error> {
            Ok(())
        }
        fn wait_vblanks(&self, _pipe: Pipe, count: u8) -> Result<(), Error> {
            let call = self.wait_calls.get();
            self.wait_calls.set(call + 1);
            if self.fail_wait_on_call.get() == Some(call) {
                return Err(Error::Unavailable(HSW_AUD_PIN_ELD_CP_VLD));
            }
            self.frames.set(self.frames.get().saturating_add(count));
            Ok(())
        }
    }

    #[test]
    fn active_link_programs_clock_pd_and_eld_then_restores_owned_fields() {
        let model = Model::default();
        let initial = model.words.borrow().clone();
        let eld = build_eld(&edid_with_audio(&[0x09, 1 << 2, 1])).unwrap();
        let mut session = Session::enable(&model, Pipe::A, 297_000, &eld).unwrap();
        assert_eq!(
            model.read32(HSW_AUD_CFG_A).unwrap() & AUD_CONFIG_PIXEL_CLOCK_HDMI_MASK,
            11 << 16
        );
        assert_eq!(
            model.read32(HSW_AUD_CFG_A).unwrap() & AUD_CONFIG_N_MASK,
            encode_n(5120)
        );
        assert_ne!(
            model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap() & AUDIO_OUTPUT_ENABLE_A,
            0
        );
        assert_eq!(
            model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap() & AUDIO_ELD_VALID_A,
            0
        );
        assert_eq!(model.frames.get(), 1);

        session.disable(&model).unwrap();
        assert_eq!(
            model.read32(AUD_PIN_BUF_CTL).unwrap(),
            initial[&AUD_PIN_BUF_CTL]
        );
        assert_eq!(
            model.read32(HSW_AUD_CFG_A).unwrap(),
            initial[&HSW_AUD_CFG_A]
        );
        assert_eq!(
            model.read32(HSW_AUD_M_CTS_ENABLE_A).unwrap(),
            initial[&HSW_AUD_M_CTS_ENABLE_A]
        );
        assert_eq!(model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap() & 0x0f, 0);
        assert_eq!(model.frames.get(), 3);
    }

    #[test]
    fn enable_store_failure_restores_landed_prefix() {
        let eld = build_eld(&edid_with_audio(&[0x09, 1 << 2, 1])).unwrap();
        for failure in 0..=4 {
            let model = Model::default();
            let initial = model.words.borrow().clone();
            model.fail_after.set(Some(failure));
            assert!(Session::enable(&model, Pipe::A, 148_500, &eld).is_err());
            model.fail_after.set(None);
            assert_eq!(
                model.read32(AUD_PIN_BUF_CTL).unwrap(),
                initial[&AUD_PIN_BUF_CTL]
            );
            assert_eq!(
                model.read32(HSW_AUD_CFG_A).unwrap(),
                initial[&HSW_AUD_CFG_A]
            );
            assert_eq!(
                model.read32(HSW_AUD_M_CTS_ENABLE_A).unwrap(),
                initial[&HSW_AUD_M_CTS_ENABLE_A]
            );
            assert_eq!(
                model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap(),
                initial[&HSW_AUD_PIN_ELD_CP_VLD]
            );
        }
    }

    #[test]
    fn disable_store_failure_retains_retryable_session_and_restores_before_return() {
        let eld = build_eld(&edid_with_audio(&[0x09, 1 << 2, 1])).unwrap();
        for failure in 0..5 {
            let model = Model::default();
            let initial = model.words.borrow().clone();
            let mut session = Session::enable(&model, Pipe::A, 148_500, &eld).unwrap();
            model.writes.borrow_mut().clear();
            model.fail_after.set(Some(failure));

            assert!(session.disable(&model).is_err(), "store {failure}");
            assert_eq!(
                model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap() & AUDIO_ELD_VALID_A,
                0,
                "ELD is invalid before any failed teardown can return"
            );
            model.fail_after.set(None);
            session.disable(&model).unwrap();
            let writes_after_success = model.writes.borrow().len();
            session.disable(&model).unwrap();
            assert_eq!(model.writes.borrow().len(), writes_after_success);

            assert_eq!(
                model.read32(AUD_PIN_BUF_CTL).unwrap(),
                initial[&AUD_PIN_BUF_CTL]
            );
            assert_eq!(
                model.read32(HSW_AUD_CFG_A).unwrap(),
                initial[&HSW_AUD_CFG_A]
            );
            assert_eq!(
                model.read32(HSW_AUD_M_CTS_ENABLE_A).unwrap(),
                initial[&HSW_AUD_M_CTS_ENABLE_A]
            );
            assert_eq!(
                model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap()
                    & (AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A),
                0
            );
            assert_eq!(
                model.frames.get(),
                if failure == 0 { 3 } else { 5 },
                "retry waits for fresh frames after any landed teardown prefix"
            );
        }
    }

    #[test]
    fn missing_vblank_rolls_back_enable_and_teardown_remains_retryable() {
        let eld = build_eld(&edid_with_audio(&[0x09, 1 << 2, 1])).unwrap();
        let model = Model::default();
        let initial = model.words.borrow().clone();
        model.fail_wait_on_call.set(Some(0));
        assert!(Session::enable(&model, Pipe::A, 148_500, &eld).is_err());
        assert_eq!(
            model.read32(AUD_PIN_BUF_CTL).unwrap(),
            initial[&AUD_PIN_BUF_CTL]
        );
        assert_eq!(
            model.read32(HSW_AUD_CFG_A).unwrap(),
            initial[&HSW_AUD_CFG_A]
        );
        assert_eq!(
            model.read32(HSW_AUD_M_CTS_ENABLE_A).unwrap(),
            initial[&HSW_AUD_M_CTS_ENABLE_A]
        );
        assert_eq!(
            model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap(),
            initial[&HSW_AUD_PIN_ELD_CP_VLD]
        );

        model.fail_wait_on_call.set(None);
        let mut session = Session::enable(&model, Pipe::A, 148_500, &eld).unwrap();
        let next_wait = model.wait_calls.get();
        model.fail_wait_on_call.set(Some(next_wait));
        assert!(session.disable(&model).is_err());
        assert_eq!(
            model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap() & AUDIO_ELD_VALID_A,
            0
        );
        assert_ne!(
            model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap() & AUDIO_OUTPUT_ENABLE_A,
            0,
            "no fresh vblank means presence detect remains on and the caller must retain link"
        );
        model.fail_wait_on_call.set(None);
        session.disable(&model).unwrap();
        assert_eq!(
            model.read32(HSW_AUD_PIN_ELD_CP_VLD).unwrap()
                & (AUDIO_OUTPUT_ENABLE_A | AUDIO_ELD_VALID_A),
            0
        );
    }
}
