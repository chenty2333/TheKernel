//! Test fixtures: synthesise EDID blocks field by field.
//!
//! Every block a test parses is built here from named fields, so a reader can
//! see exactly what is being claimed.  Nothing in this module is compiled into
//! the product: it exists so the parser tests can state "a block whose
//! standard timing code is 0x3140" instead of pasting a hexadecimal blob whose
//! meaning nobody can check.

use alloc::vec::Vec;

use super::edid::BLOCK_LEN;

/// A detailed timing descriptor, in the parameterisation monitors use.
#[derive(Clone, Copy, Debug)]
pub struct DetailedTimingSpec {
    pub clock_khz: u32,
    pub hdisplay: u16,
    pub hblank: u16,
    pub hfront: u16,
    pub hsync: u16,
    pub vdisplay: u16,
    pub vblank: u16,
    pub vfront: u16,
    pub vsync: u16,
    pub hsync_positive: bool,
    pub vsync_positive: bool,
    pub interlaced: bool,
    pub width_mm: u16,
    pub height_mm: u16,
    pub hborder: u8,
    pub vborder: u8,
}

impl DetailedTimingSpec {
    /// A 1920x1080@60 timing, the shape a modern panel's preferred descriptor
    /// has.  Tests override the fields they are about.
    pub const fn new(clock_khz: u32, hdisplay: u16, vdisplay: u16) -> Self {
        Self {
            clock_khz,
            hdisplay,
            hblank: 280,
            hfront: 88,
            hsync: 44,
            vdisplay,
            vblank: 45,
            vfront: 4,
            vsync: 5,
            hsync_positive: true,
            vsync_positive: true,
            interlaced: false,
            width_mm: 344,
            height_mm: 194,
            hborder: 0,
            vborder: 0,
        }
    }

    /// Encodes the 18 bytes, exactly inverting the parser's decoding.
    pub fn encode(&self) -> [u8; 18] {
        let mut data = [0u8; 18];
        let clock_10khz = (self.clock_khz / 10) as u16;
        data[0..2].copy_from_slice(&clock_10khz.to_le_bytes());
        data[2] = self.hdisplay as u8;
        data[3] = self.hblank as u8;
        data[4] = ((self.hdisplay >> 8) as u8) << 4 | ((self.hblank >> 8) as u8 & 0x0f);
        data[5] = self.vdisplay as u8;
        data[6] = self.vblank as u8;
        data[7] = ((self.vdisplay >> 8) as u8) << 4 | ((self.vblank >> 8) as u8 & 0x0f);
        data[8] = self.hfront as u8;
        data[9] = self.hsync as u8;
        data[10] = ((self.vfront & 0x0f) as u8) << 4 | (self.vsync as u8 & 0x0f);
        data[11] = (((self.hfront >> 8) as u8) & 0x3) << 6
            | (((self.hsync >> 8) as u8) & 0x3) << 4
            | (((self.vfront >> 4) as u8) & 0x3) << 2
            | ((self.vsync >> 4) as u8 & 0x3);
        data[12] = self.width_mm as u8;
        data[13] = self.height_mm as u8;
        data[14] = ((self.width_mm >> 8) as u8) << 4 | ((self.height_mm >> 8) as u8 & 0x0f);
        data[15] = self.hborder;
        data[16] = self.vborder;
        data[17] = (self.interlaced as u8) << 7
            | 0x18 // digital separate sync
            | (self.vsync_positive as u8) << 2
            | (self.hsync_positive as u8) << 1;
        data
    }
}

/// A display range limits descriptor.
#[derive(Clone, Copy, Debug)]
pub struct RangeLimitsSpec {
    pub min_vertical_hz: u8,
    pub max_vertical_hz: u8,
    pub min_horizontal_khz: u8,
    pub max_horizontal_khz: u8,
    /// Maximum pixel clock in units of 10 MHz.
    pub max_clock_10mhz: u8,
    /// Extended timing information type: 0x00 default GTF, 0x01 bare limits,
    /// 0x02 secondary GTF, 0x04 CVT.
    pub class: u8,
    /// CVT: coarse maximum clock adjustment in units of 250 kHz, bits 7..2.
    pub cvt_clock_step_250khz: u8,
    /// CVT: maximum horizontal pixels, in units of 8.
    pub cvt_max_horizontal_8px: u16,
    pub cvt_version: u8,
    pub cvt_revision: u8,
    /// CVT: aspect ratio support bits (7 = 4:3, 6 = 16:9, 5 = 16:10,
    /// 4 = 5:4, 3 = 15:9).
    pub cvt_aspects: u8,
    /// CVT byte 15: preferred aspect in bits 7..5, standard blanking in bit
    /// 3, reduced blanking in bit 4.
    pub cvt_flags: u8,
    pub cvt_scaling: u8,
    pub cvt_preferred_refresh_hz: u8,
}

impl RangeLimitsSpec {
    /// A range a modern panel states, with CVT support and reduced blanking.
    pub const fn cvt_panel() -> Self {
        Self {
            min_vertical_hz: 48,
            max_vertical_hz: 75,
            min_horizontal_khz: 30,
            max_horizontal_khz: 90,
            max_clock_10mhz: 17, // 170 MHz
            class: 0x04,
            cvt_clock_step_250khz: 0,
            cvt_max_horizontal_8px: 0,
            cvt_version: 1,
            cvt_revision: 1,
            cvt_aspects: 0b1111_0000,
            // Preferred aspect 16:10 (bits 7..5 = 010), standard blanking
            // (bit 3) and reduced blanking (bit 4) both supported.
            cvt_flags: 0b0100_0000 | 0x18,
            cvt_scaling: 0,
            cvt_preferred_refresh_hz: 60,
        }
    }

    /// A GTF range with no formula preference, the older shape.
    pub const fn gtf() -> Self {
        Self {
            class: 0x00,
            ..Self::cvt_panel()
        }
    }

    pub fn encode(&self) -> [u8; 18] {
        let mut data = [0u8; 18];
        data[3] = 0xfd; // display range limits
        data[4] = 0; // no offsets
        data[5] = self.min_vertical_hz;
        data[6] = self.max_vertical_hz;
        data[7] = self.min_horizontal_khz;
        data[8] = self.max_horizontal_khz;
        data[9] = self.max_clock_10mhz;
        data[10] = self.class;
        match self.class {
            0x04 => {
                data[11] = self.cvt_version << 4 | (self.cvt_revision & 0x0f);
                let horizontal = self.cvt_max_horizontal_8px / 8;
                data[12] = (self.cvt_clock_step_250khz & 0x3f) << 2
                    | ((horizontal >> 8) as u8 & 0x3);
                data[13] = horizontal as u8;
                data[14] = self.cvt_aspects;
                data[15] = self.cvt_flags;
                data[16] = self.cvt_scaling;
                data[17] = self.cvt_preferred_refresh_hz;
            }
            0x00 | 0x01 => {
                data[11] = 0x0a;
                for byte in &mut data[12..18] {
                    *byte = 0x20;
                }
            }
            0x02 => {
                data[12] = 30; // 60 kHz start frequency
                data[13] = 80; // C = 40
                data[14] = 0x40; // M low
                data[15] = 0x0c; // M high
                data[16] = 128; // K
                data[17] = 40; // J = 20
            }
            _ => {
                for byte in &mut data[11..18] {
                    *byte = 0x20;
                }
            }
        }
        data
    }
}

/// Builds a base block from named fields.
#[derive(Clone, Debug)]
pub struct BaseBlockBuilder {
    block: [u8; BLOCK_LEN],
}

impl Default for BaseBlockBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseBlockBuilder {
    /// A digital EDID 1.4 base block with no timings and no extensions.
    pub fn new() -> Self {
        let mut block = [0u8; BLOCK_LEN];
        block[0..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        block[0x12] = 1; // version
        block[0x13] = 4; // revision
        block[0x14] = 0xa5; // digital, 8 bits per colour, DisplayPort
        block[0x15] = 34; // 34 cm
        block[0x16] = 19; // 19 cm
        block[0x17] = 120; // gamma 2.20
        // YCbCr 4:4:4 accepted, preferred timing is native, continuous
        // frequency; sRGB is deliberately left clear so tests can see the bit.
        block[0x18] = 0x0b;
        for slot in 0..4 {
            // An unused descriptor is a dummy: zero clock, tag 0x10.
            block[descriptor_offset(slot) + 3] = 0x10;
        }
        BaseBlockBuilder { block }
    }

    pub fn manufacturer(mut self, pnp: &[u8; 3]) -> Self {
        let raw = (u16::from(pnp[0] - b'A' + 1) << 10)
            | (u16::from(pnp[1] - b'A' + 1) << 5)
            | u16::from(pnp[2] - b'A' + 1);
        self.block[0x08] = (raw >> 8) as u8;
        self.block[0x09] = raw as u8;
        self
    }

    pub fn product_code(mut self, code: u16) -> Self {
        self.block[0x0a] = code as u8;
        self.block[0x0b] = (code >> 8) as u8;
        self
    }

    pub fn serial_number(mut self, serial: u32) -> Self {
        self.block[0x0c..0x10].copy_from_slice(&serial.to_le_bytes());
        self
    }

    pub fn manufacture(mut self, week: u8, year: u16) -> Self {
        self.block[0x10] = week;
        self.block[0x11] = (year - 1990) as u8;
        self
    }

    pub fn version(mut self, major: u8, revision: u8) -> Self {
        self.block[0x12] = major;
        self.block[0x13] = revision;
        self
    }

    pub fn analog_input(mut self, byte: u8) -> Self {
        self.block[0x14] = byte & 0x7f;
        self
    }

    pub fn screen_size_cm(mut self, width: u8, height: u8) -> Self {
        self.block[0x15] = width;
        self.block[0x16] = height;
        self
    }

    /// Sets the gamma byte the way a sink encodes it: `gamma * 100 - 100`.
    pub fn gamma_byte(mut self, byte: u8) -> Self {
        self.block[0x17] = byte;
        self
    }

    pub fn features(mut self, byte: u8) -> Self {
        self.block[0x18] = byte;
        self
    }

    /// Sets a chromaticity coordinate from its 10-bit encoding.
    pub fn chromaticity(mut self, raw: [u16; 8]) -> Self {
        // Order: red x, red y, green x, green y, blue x, blue y, white x,
        // white y; the two low-bit bytes come first in the block.
        self.block[0x19] = ((raw[0] & 0x3) << 6
            | (raw[1] & 0x3) << 4
            | (raw[2] & 0x3) << 2
            | (raw[3] & 0x3)) as u8;
        self.block[0x1a] = ((raw[4] & 0x3) << 6
            | (raw[5] & 0x3) << 4
            | (raw[6] & 0x3) << 2
            | (raw[7] & 0x3)) as u8;
        for (index, value) in raw.iter().enumerate() {
            self.block[0x1b + index] = (value >> 2) as u8;
        }
        self
    }

    pub fn established_i_ii(mut self, bits: [u8; 3]) -> Self {
        self.block[0x23..0x26].copy_from_slice(&bits);
        self
    }

    /// Writes a standard timing identification entry from its two bytes.
    pub fn standard_timing_code(mut self, slot: usize, code: [u8; 2]) -> Self {
        let offset = 0x26 + slot * 2;
        self.block[offset] = code[0];
        self.block[offset + 1] = code[1];
        self
    }

    /// Writes a standard timing entry from the fields it stands for.
    pub fn standard_timing(
        mut self,
        slot: usize,
        hdisplay: u16,
        aspect_bits: u8,
        refresh_hz: u16,
    ) -> Self {
        let code = [
            (hdisplay / 8) as u8 - 31,
            (aspect_bits & 0x3) << 6 | ((refresh_hz - 60) as u8 & 0x3f),
        ];
        self = self.standard_timing_code(slot, code);
        self
    }

    pub fn descriptor(mut self, slot: usize, data: [u8; 18]) -> Self {
        let offset = descriptor_offset(slot);
        self.block[offset..offset + 18].copy_from_slice(&data);
        self
    }

    pub fn detailed_timing(self, slot: usize, spec: &DetailedTimingSpec) -> Self {
        self.descriptor(slot, spec.encode())
    }

    pub fn range_limits(self, slot: usize, spec: &RangeLimitsSpec) -> Self {
        self.descriptor(slot, spec.encode())
    }

    /// Writes a monitor name into a 0xFC descriptor.
    pub fn monitor_name(mut self, slot: usize, name: &[u8]) -> Self {
        let mut data = [0u8; 18];
        data[3] = 0xfc;
        for (index, byte) in data[5..18].iter_mut().enumerate() {
            *byte = *name.get(index).unwrap_or(&b' ');
        }
        if name.len() < 13 {
            data[5 + name.len()] = b'\n';
        }
        self = self.descriptor(slot, data);
        self
    }

    /// Writes a standard-timings display descriptor (sub-tag 0xFA).
    pub fn standard_timing_ids_descriptor(mut self, slot: usize, codes: [[u8; 2]; 6]) -> Self {
        let mut data = [0u8; 18];
        data[3] = 0xfa;
        for (index, code) in codes.iter().enumerate() {
            data[5 + index * 2] = code[0];
            data[6 + index * 2] = code[1];
        }
        data[17] = 0x0a;
        self = self.descriptor(slot, data);
        self
    }

    /// Writes an established-timings-III descriptor (sub-tag 0xF7).
    pub fn established_timings_iii(mut self, slot: usize, bits: [u8; 6]) -> Self {
        let mut data = [0u8; 18];
        data[3] = 0xf7;
        data[5] = 0x0a;
        data[6..12].copy_from_slice(&bits);
        self = self.descriptor(slot, data);
        self
    }

    /// Writes a CVT three-byte timing codes descriptor (sub-tag 0xF8).
    pub fn cvt_timing_codes(mut self, slot: usize, version: u8, codes: [[u8; 3]; 4]) -> Self {
        let mut data = [0u8; 18];
        data[3] = 0xf8;
        data[5] = version;
        for (index, code) in codes.iter().enumerate() {
            data[6 + index * 3..9 + index * 3].copy_from_slice(code);
        }
        self = self.descriptor(slot, data);
        self
    }

    /// Sets the extension block count the base block declares.
    pub fn extension_count(mut self, count: u8) -> Self {
        self.block[0x7e] = count;
        self
    }

    /// Returns the block with a correct checksum.
    pub fn build(self) -> [u8; BLOCK_LEN] {
        let mut block = self.block;
        block[BLOCK_LEN - 1] = 0;
        block[BLOCK_LEN - 1] = checksum_byte(&block);
        block
    }

    /// Returns the block with a deliberately wrong checksum.
    ///
    /// The correct byte is flipped rather than replaced, so the result is a
    /// bad block whatever the block's contents happen to sum to.
    pub fn build_with_bad_checksum(self) -> [u8; BLOCK_LEN] {
        let mut block = self.build();
        block[BLOCK_LEN - 1] ^= 0x01;
        block
    }
}

/// Builds a CTA-861 extension block from named fields.
#[derive(Clone, Debug)]
pub struct CtaBlockBuilder {
    revision: u8,
    flags: u8,
    data_blocks: Vec<u8>,
    detailed_timings: Vec<[u8; 18]>,
}

impl CtaBlockBuilder {
    pub fn new(revision: u8) -> Self {
        CtaBlockBuilder {
            revision,
            flags: 0,
            data_blocks: Vec::new(),
            detailed_timings: Vec::new(),
        }
    }

    pub fn underscan(mut self, value: bool) -> Self {
        self.flags = self.flags & !0x80 | (u8::from(value) << 7);
        self
    }

    pub fn basic_audio(mut self, value: bool) -> Self {
        self.flags = self.flags & !0x40 | (u8::from(value) << 6);
        self
    }

    pub fn ycbcr444(mut self, value: bool) -> Self {
        self.flags = self.flags & !0x20 | (u8::from(value) << 5);
        self
    }

    pub fn ycbcr422(mut self, value: bool) -> Self {
        self.flags = self.flags & !0x10 | (u8::from(value) << 4);
        self
    }

    pub fn native_dtd_count(mut self, count: u8) -> Self {
        self.flags = self.flags & 0xf0 | (count & 0x0f);
        self
    }

    /// Appends a data block with an explicit tag and payload.
    pub fn data_block(mut self, tag: u8, payload: &[u8]) -> Self {
        assert!(payload.len() <= 31, "a data block carries at most 31 bytes");
        self.data_blocks.push(tag << 5 | payload.len() as u8);
        self.data_blocks.extend_from_slice(payload);
        self
    }

    /// Appends a video data block of plain VIC codes.
    pub fn vics(self, vics: &[u8]) -> Self {
        let payload: Vec<u8> = vics.iter().map(|vic| vic & 0x7f).collect();
        self.data_block(2, &payload)
    }

    /// Appends a video data block marking the listed codes native.
    pub fn native_vics(self, vics: &[u8]) -> Self {
        let payload: Vec<u8> = vics.iter().map(|vic| vic | 0x80).collect();
        self.data_block(2, &payload)
    }

    /// Appends an audio data block of one linear PCM stereo format.
    pub fn lpcm_audio(self) -> Self {
        self.data_block(1, &[0x09, 0x7f, 0x07])
    }

    pub fn detailed_timing(mut self, spec: &DetailedTimingSpec) -> Self {
        self.detailed_timings.push(spec.encode());
        self
    }

    /// Returns the block with a correct checksum.
    pub fn build(self) -> [u8; BLOCK_LEN] {
        self.build_with_checksum(true)
    }

    /// Returns the block with a deliberately wrong checksum.
    pub fn build_with_bad_checksum(self) -> [u8; BLOCK_LEN] {
        self.build_with_checksum(false)
    }

    fn build_with_checksum(self, valid: bool) -> [u8; BLOCK_LEN] {
        let mut block = [0u8; BLOCK_LEN];
        // CTA-861 extension header: tag, revision, detailed timing offset,
        // flags - the offset is written below once it is known.
        block[0] = 0x02;
        block[1] = self.revision;
        block[3] = self.flags;
        let mut cursor = 4usize;
        if !self.data_blocks.is_empty() {
            block[cursor..cursor + self.data_blocks.len()]
                .copy_from_slice(&self.data_blocks);
            cursor += self.data_blocks.len();
        }
        let dtd_offset = if self.detailed_timings.is_empty() {
            // With no detailed timings the offset points just past the data
            // block collection; a value of zero means the block describes
            // nothing at all, which is not what a sink with modes sends.
            if self.data_blocks.is_empty() { 0 } else { cursor }
        } else {
            // CTA-861 puts the timing area wherever the offset says; the data
            // blocks are written contiguously, so no padding is needed.
            cursor
        };
        block[2] = dtd_offset as u8;
        let mut cursor = dtd_offset;
        for timing in &self.detailed_timings {
            assert!(cursor + 18 <= BLOCK_LEN - 1, "extension is full");
            block[cursor..cursor + 18].copy_from_slice(timing);
            cursor += 18;
        }
        if valid {
            block[BLOCK_LEN - 1] = checksum_byte(&block);
        } else {
            block[BLOCK_LEN - 1] = checksum_byte(&block) ^ 0x01;
        }
        block
    }
}

/// Concatenates a base block and its extensions into one EDID buffer.
pub fn assemble(base: [u8; BLOCK_LEN], extensions: &[[u8; BLOCK_LEN]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(BLOCK_LEN * (1 + extensions.len()));
    bytes.extend_from_slice(&base);
    for extension in extensions {
        bytes.extend_from_slice(extension);
    }
    bytes
}

fn descriptor_offset(slot: usize) -> usize {
    0x36 + slot * 18
}

/// The byte that makes a block sum to zero modulo 256.
pub fn checksum_byte(block: &[u8; BLOCK_LEN]) -> u8 {
    block
        .iter()
        .take(BLOCK_LEN - 1)
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        .wrapping_neg()
}

/// A small deterministic generator, so the property test is reproducible.
///
/// The repository has no property-testing framework; a fixed-seed generator
/// keeps the test hermetic and makes a failure reproducible from the seed
/// printed in the assertion message.
#[derive(Clone, Copy, Debug)]
pub struct Rng(u32);

impl Rng {
    pub const fn new(seed: u32) -> Self {
        Rng(seed | 1)
    }

    pub fn next_u32(&mut self) -> u32 {
        // xorshift32: deterministic, no dependencies, good enough to spread
        // mutations over a block.
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 17;
        value ^= value << 5;
        self.0 = value;
        value
    }

    pub fn below(&mut self, limit: usize) -> usize {
        if limit == 0 {
            0
        } else {
            self.next_u32() as usize % limit
        }
    }

    pub fn byte(&mut self) -> u8 {
        self.next_u32() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixtures must be exactly invertible by the parser, otherwise every
    /// parser test would be asserting the fixture's bug instead.
    #[test]
    fn detailed_timing_spec_round_trips_through_the_encoding() {
        let spec = DetailedTimingSpec {
            clock_khz: 148_500,
            hdisplay: 1920,
            hblank: 280,
            hfront: 88,
            hsync: 44,
            vdisplay: 1080,
            vblank: 45,
            vfront: 4,
            vsync: 5,
            hsync_positive: true,
            vsync_positive: false,
            interlaced: false,
            width_mm: 344,
            height_mm: 194,
            hborder: 0,
            vborder: 0,
        };
        let bytes = spec.encode();
        let parsed = crate::drm::modes::edid::parse_detailed_timing_for_test(&bytes);
        assert_eq!(parsed.mode.clock_khz, spec.clock_khz);
        assert_eq!(parsed.mode.hdisplay, spec.hdisplay);
        assert_eq!(parsed.mode.hblank(), spec.hblank);
        assert_eq!(parsed.mode.hsync_start, spec.hdisplay + spec.hfront);
        assert_eq!(parsed.mode.hsync_len(), spec.hsync);
        assert_eq!(parsed.mode.vdisplay, spec.vdisplay);
        assert_eq!(parsed.mode.vblank(), spec.vblank);
        assert_eq!(parsed.mode.vsync_start, spec.vdisplay + spec.vfront);
        assert_eq!(parsed.mode.vsync_len(), spec.vsync);
        assert!(parsed.mode.hsync_positive);
        assert!(!parsed.mode.vsync_positive);
        assert_eq!(parsed.image_size_mm, Some((344, 194)));
    }

    #[test]
    fn checksums_are_correct_when_asked_for() {
        let block = BaseBlockBuilder::new().build();
        assert_eq!(block.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        let broken = BaseBlockBuilder::new().build_with_bad_checksum();
        assert_ne!(broken.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        let cta = CtaBlockBuilder::new(3).vics(&[16, 4]).build();
        assert_eq!(cta.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
        let broken = CtaBlockBuilder::new(3).vics(&[16]).build_with_bad_checksum();
        assert_ne!(broken.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
    }

    #[test]
    fn the_deterministic_generator_does_not_get_stuck() {
        let mut rng = Rng::new(0x1234_5678);
        let first = rng.next_u32();
        let mut distinct = 1;
        for _ in 0..1000 {
            if rng.next_u32() != first {
                distinct += 1;
            }
        }
        assert!(distinct > 900, "the generator repeats too often");
        assert!(rng.below(1) == 0);
        assert!(rng.below(0) == 0);
    }
}
