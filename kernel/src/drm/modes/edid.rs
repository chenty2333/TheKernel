//! EDID parsing.
//!
//! This module turns the bytes a monitor sends down a DDC channel into typed
//! facts.  It parses the 128-byte base block in full - header, checksum,
//! vendor and product identification, version, video input definition, screen
//! size, gamma, feature byte, chromaticity, established timings, standard
//! timings and all four 18-byte descriptors - and the extension blocks that
//! follow it, decoding CTA-861 (which is how an HDMI sink advertises its
//! modes) and validating every extension block's checksum.
//!
//! Three properties matter more than completeness here, because these bytes
//! arrive over a cable and the machine this runs on has no serial port to
//! report a crash on:
//!
//! * **No panic, ever.**  Every read is bounds-checked against the block
//!   structure and every loop is bounded by the number of blocks the EDID
//!   declares (at most 256), never by a length taken from the data.  The
//!   parsed base block is copied into owned fields, so accessors cannot index
//!   out of range after a successful parse.
//! * **No allocation.**  The parser borrows the caller's buffer, copies
//!   fixed-size fields into the [`Edid`] value, and parses extensions on
//!   demand, so a malformed block cannot cause an allocation blow-up.
//! * **Structured errors.**  [`Edid::parse`] is strict: a bad header, a bad
//!   checksum, a missing declared extension block, a malformed descriptor or
//!   a malformed CTA-861 collection is a typed [`EdidError`], never a
//!   silently accepted block.  [`Edid::parse_lossy`] exists for bring-up on
//!   real hardware: it records what it skipped in [`EdidWarnings`], so a
//!   decision made from a damaged EDID is visible in the boot log instead of
//!   being implicit.
//!
//! Extensions this parser does not understand (DisplayID, block maps, vendor
//! blocks, CTA revisions before 3) are still checked for a valid checksum,
//! then exposed as [`Extension::Unknown`] and ignored when enumerating modes.
//! They never fail the parse, and they are never half-interpreted.

use core::{fmt, str};

use super::mode::{Mode, ModeFlags, TimingSource};

/// Every EDID block is 128 bytes, base block and extensions alike.
pub const BLOCK_LEN: usize = 128;
/// The number of extension blocks a base block can declare (one byte).
pub const MAX_EXTENSION_BLOCKS: u8 = 255;
/// Detailed timing descriptors in the base block.
pub const DESCRIPTOR_COUNT: usize = 4;
/// Standard timing identification entries in the base block.
pub const STANDARD_TIMING_COUNT: usize = 8;
/// Offset of the first 18-byte descriptor in the base block.
const DESCRIPTOR_OFFSET: usize = 0x36;
const DESCRIPTOR_LEN: usize = 18;
/// The last byte of a block is its checksum.
const CHECKSUM_OFFSET: usize = BLOCK_LEN - 1;

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// CTA-861 extension block tag.
const EXT_TAG_CTA861: u8 = 0x02;
/// The first CTA revision that defines the data block collection.
const CTA_REVISION_DATA_BLOCKS: u8 = 3;

/// Standard timing codes that carry no timing information.
const STANDARD_TIMING_UNUSED: [[u8; 2]; 3] = [[0x01, 0x01], [0x00, 0x00], [0x20, 0x20]];

/// Frames an interlaced EDID detailed timing may describe in field lines.
///
/// A descriptor for 1080i is normally written with a vertical active of 540
/// lines and the interlace flag set, while the frame it describes has 1080
/// lines.  Only the formats CTA-861 defines as interlaced are converted, so a
/// sink that describes something else keeps the units it wrote.
const INTERLACED_FRAMES: &[(u16, u16)] = &[
    (1920, 1080),
    (2880, 480),
    (1440, 480),
    (720, 480),
    (2880, 576),
    (1440, 576),
    (720, 576),
];

/// Why an EDID could not be parsed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EdidError {
    /// The buffer is empty.
    Empty,
    /// The buffer length is not a whole number of 128-byte blocks.
    NotBlockAligned { len: usize },
    /// The buffer holds more blocks than the addressing scheme can carry.
    TooManyBlocks { blocks: usize },
    /// The first eight bytes are not the EDID header.
    BadHeader,
    /// The base block's bytes do not sum to zero modulo 256.
    BadBaseChecksum { sum: u8 },
    /// An extension block's bytes do not sum to zero modulo 256.
    BadExtensionChecksum { block: u8, sum: u8 },
    /// The base block declares extension blocks the caller did not supply.
    MissingExtensionBlocks { declared: u8, present: u8 },
    /// The EDID major version is not 1.
    UnsupportedVersion { major: u8, revision: u8 },
    /// A detailed timing descriptor cannot be programmed.
    InvalidDetailedTiming { index: u8 },
    /// A display descriptor is internally inconsistent.
    InvalidDisplayDescriptor { index: u8 },
    /// A CTA-861 extension's data block collection runs past its block.
    MalformedCtaDataBlocks { block: u8 },
    /// A CTA-861 extension's timing area overlaps its data blocks or holds a
    /// descriptor that cannot be programmed.
    MalformedCtaDetailedTimings { block: u8 },
}

impl fmt::Display for EdidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EdidError::Empty => f.write_str("empty EDID buffer"),
            EdidError::NotBlockAligned { len } => {
                write!(f, "EDID length {len} is not a multiple of {BLOCK_LEN}")
            }
            EdidError::TooManyBlocks { blocks } => {
                write!(f, "EDID holds {blocks} blocks, the maximum is 256")
            }
            EdidError::BadHeader => f.write_str("EDID header is not 00 FF FF FF FF FF FF 00"),
            EdidError::BadBaseChecksum { sum } => {
                write!(f, "base block checksum is wrong (sum {sum:#04x})")
            }
            EdidError::BadExtensionChecksum { block, sum } => {
                write!(
                    f,
                    "extension block {block} checksum is wrong (sum {sum:#04x})"
                )
            }
            EdidError::MissingExtensionBlocks { declared, present } => write!(
                f,
                "base block declares {declared} extension blocks, {present} were supplied"
            ),
            EdidError::UnsupportedVersion { major, revision } => {
                write!(f, "unsupported EDID version {major}.{revision}")
            }
            EdidError::InvalidDetailedTiming { index } => {
                write!(f, "detailed timing descriptor {index} is not programmable")
            }
            EdidError::InvalidDisplayDescriptor { index } => {
                write!(f, "display descriptor {index} is inconsistent")
            }
            EdidError::MalformedCtaDataBlocks { block } => {
                write!(f, "CTA-861 extension block {block} has a malformed data block")
            }
            EdidError::MalformedCtaDetailedTimings { block } => {
                write!(f, "CTA-861 extension block {block} has a malformed timing area")
            }
        }
    }
}

impl fmt::Debug for EdidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// What a lenient parse had to skip.
///
/// A clean EDID reports zero in every field.  Anything else must be logged by
/// the caller: a mode chosen from a damaged EDID is a decision, and decisions
/// made at boot on a machine with no serial port have to be visible.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct EdidWarnings {
    pub bad_extension_checksums: u8,
    pub missing_extension_blocks: u8,
    pub invalid_descriptors: u8,
    pub malformed_cta_blocks: u8,
}

impl EdidWarnings {
    pub fn is_clean(&self) -> bool {
        self.count() == 0
    }

    pub fn count(&self) -> u32 {
        u32::from(self.bad_extension_checksums)
            + u32::from(self.missing_extension_blocks)
            + u32::from(self.invalid_descriptors)
            + u32::from(self.malformed_cta_blocks)
    }
}

impl fmt::Display for EdidWarnings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_clean() {
            return f.write_str("clean");
        }
        write!(
            f,
            "bad-extension-checksums={} missing-extensions={} invalid-descriptors={} \
             malformed-cta={}",
            self.bad_extension_checksums,
            self.missing_extension_blocks,
            self.invalid_descriptors,
            self.malformed_cta_blocks
        )
    }
}

impl fmt::Debug for EdidWarnings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// How much of a damaged EDID a parse is willing to accept.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ParseOptions {
    /// Fail when the buffer holds fewer blocks than the base block declares.
    require_declared_extensions: bool,
    /// Fail on an extension block whose checksum is wrong.
    require_valid_extension_checksums: bool,
    /// Fail on a descriptor that cannot be decoded.
    require_valid_descriptors: bool,
    /// Fail on a CTA-861 extension whose collections do not parse.
    require_valid_cta_blocks: bool,
}

impl ParseOptions {
    const STRICT: ParseOptions = ParseOptions {
        require_declared_extensions: true,
        require_valid_extension_checksums: true,
        require_valid_descriptors: true,
        require_valid_cta_blocks: true,
    };

    const LOSSY: ParseOptions = ParseOptions {
        require_declared_extensions: false,
        require_valid_extension_checksums: false,
        require_valid_descriptors: false,
        require_valid_cta_blocks: false,
    };
}

/// A manufacturer's three-letter PNP identifier.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PnpId([u8; 3]);

impl PnpId {
    pub fn as_bytes(&self) -> &[u8; 3] {
        &self.0
    }

    /// The identifier as text.  Every byte is offset from `A`, so the result
    /// is valid UTF-8 by construction.
    pub fn as_str(&self) -> &str {
        str::from_utf8(&self.0).unwrap_or("???")
    }
}

impl fmt::Display for PnpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for PnpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PnpId({})", self.as_str())
    }
}

/// Identification fields from the base block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VendorProduct {
    pub manufacturer: PnpId,
    pub product_code: u16,
    pub serial_number: u32,
    /// Week of manufacture, 0 when the sink only encodes a year.
    pub week: u8,
    /// Year of manufacture, 0 when the sink does not encode one.
    pub year: u16,
    /// True when the week field carries a model year rather than a week.
    pub model_year: bool,
}

/// The video input definition byte, interpreted for digital and analog sinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoInput {
    Digital(DigitalInput),
    Analog(AnalogInput),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigitalInput {
    /// Bits per colour component, when the sink declares it.
    pub bit_depth: Option<u8>,
    pub interface: DigitalInterface,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DigitalInterface {
    Undefined,
    Dvi,
    HdmiA,
    HdmiB,
    Mddi,
    DisplayPort,
    Reserved(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnalogInput {
    /// Signal level standard: 0 = 0.700/0.300, 1 = 0.714/0.286,
    /// 2 = 1.000/0.400, 3 = 0.700/0.000.
    pub level: u8,
    /// Blank-to-black setup is expected.
    pub video_setup: bool,
    pub separate_sync: bool,
    pub composite_sync: bool,
    pub sync_on_green: bool,
    /// Serration is required on the vertical sync pulse.
    pub serration: bool,
}

/// Screen size, in millimetres when the sink states it in centimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenSize {
    /// Width in millimetres; `None` when the sink encodes an aspect ratio
    /// instead (EDID 1.4, one of the two bytes zero) or nothing at all.
    pub width_mm: Option<u16>,
    pub height_mm: Option<u16>,
    /// Landscape aspect ratio times 100, when the sink encodes only that.
    pub landscape_aspect_ratio_percent: Option<u16>,
    /// Portrait aspect ratio times 100.
    pub portrait_aspect_ratio_percent: Option<u16>,
}

/// The feature support byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    pub standby: bool,
    pub suspend: bool,
    pub active_off: bool,
    pub srgb_is_default: bool,
    /// EDID 1.4: the preferred timing uses the sink's native pixel format and
    /// refresh rate.  EDID 1.3 spells the same bit "preferred timing is
    /// native"; older revisions use it for "default GTF supported".
    pub preferred_timing_is_native: bool,
    /// EDID 1.4: the sink accepts continuous frequency timings.  EDID 1.3:
    /// default GTF supported.
    pub continuous_frequency: bool,
    /// Bits 4..3 as written: for an analog sink the standard's display colour
    /// type, for a digital sink the colour encodings it accepts.
    pub color_bits: u8,
    /// Digital sinks of revision 4 and later accept RGB 4:4:4 always, and
    /// state these two encodings with the feature byte.
    pub ycbcr444: bool,
    pub ycbcr422: bool,
}

/// Chromaticity coordinates, as the 10-bit values the block carries.
///
/// The standard's encoding is `raw / 1024`; [`Chromaticity::permille`] applies
/// that without floating point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chromaticity {
    pub red_x: u16,
    pub red_y: u16,
    pub green_x: u16,
    pub green_y: u16,
    pub blue_x: u16,
    pub blue_y: u16,
    pub white_x: u16,
    pub white_y: u16,
}

impl Chromaticity {
    /// Converts a 10-bit coordinate to thousandths.
    pub const fn permille(raw: u16) -> u16 {
        (raw * 1000 + 512) / 1024
    }

    /// True when the sink left every primary coordinate unset, which the
    /// standard defines as "no coordinates given" rather than "black".
    pub fn primaries_unset(&self) -> bool {
        self.red_x == 0
            && self.red_y == 0
            && self.green_x == 0
            && self.green_y == 0
            && self.blue_x == 0
            && self.blue_y == 0
    }
}

/// The established timings bitmaps (I, II and, when present, III).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EstablishedTimings {
    /// Bytes 0x23..0x26: bitmaps I and II.
    pub i_ii: [u8; 3],
    /// The six bytes of an established-timings-III display descriptor.
    pub iii: Option<[u8; 6]>,
}

/// Aspect ratio of an EDID standard timing identification code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AspectRatio {
    Ratio16To10,
    Ratio4To3,
    Ratio5To4,
    Ratio16To9,
}

impl AspectRatio {
    fn from_bits(bits: u8) -> AspectRatio {
        match bits & 0x3 {
            0 => AspectRatio::Ratio16To10,
            1 => AspectRatio::Ratio4To3,
            2 => AspectRatio::Ratio5To4,
            _ => AspectRatio::Ratio16To9,
        }
    }

    /// Vertical size for a horizontal size, truncated the way the standard's
    /// integer arithmetic does.
    pub const fn vertical_for(self, hdisplay: u16) -> u16 {
        match self {
            AspectRatio::Ratio16To10 => hdisplay * 10 / 16,
            AspectRatio::Ratio4To3 => hdisplay * 3 / 4,
            AspectRatio::Ratio5To4 => hdisplay * 4 / 5,
            AspectRatio::Ratio16To9 => hdisplay * 9 / 16,
        }
    }
}

/// One two-byte standard timing identification entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandardTiming {
    /// A timing named by resolution and refresh rate.  The exact blanking is
    /// not in the descriptor: E-EDID 1.4 Appendix B requires a DMT lookup
    /// first (see [`super::dmt::by_std_id`]) and a formula only as a fallback.
    Named {
        code: [u8; 2],
        hdisplay: u16,
        vdisplay: u16,
        refresh_hz: u16,
        aspect: AspectRatio,
    },
    /// 0x0101, 0x0000 or 0x2020: this entry carries no timing.
    Unused { code: [u8; 2] },
    /// A code whose first byte is zero, which the standard reserves.
    Reserved { code: [u8; 2] },
}

impl StandardTiming {
    fn parse(code: [u8; 2]) -> StandardTiming {
        if STANDARD_TIMING_UNUSED.contains(&code) {
            return StandardTiming::Unused { code };
        }
        if code[0] == 0 {
            return StandardTiming::Reserved { code };
        }
        let hdisplay = (u16::from(code[0]) + 31) * 8;
        let aspect = AspectRatio::from_bits(code[1] >> 6);
        StandardTiming::Named {
            code,
            hdisplay,
            vdisplay: aspect.vertical_for(hdisplay),
            refresh_hz: u16::from(code[1] & 0x3f) + 60,
            aspect,
        }
    }

    /// Decodes a two-byte code into `(hdisplay, vdisplay, refresh_hz)`
    /// without building the enum.  Returns `None` for the codes that carry no
    /// timing information and for the reserved ones.
    pub fn named_parts(code: [u8; 2]) -> Option<(u16, u16, u16)> {
        match StandardTiming::parse(code) {
            StandardTiming::Named {
                hdisplay,
                vdisplay,
                refresh_hz,
                ..
            } => Some((hdisplay, vdisplay, refresh_hz)),
            _ => None,
        }
    }
}

/// A short ASCII string from a display descriptor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EdidText {
    bytes: [u8; 13],
    len: u8,
}

impl EdidText {
    fn parse(raw: &[u8]) -> EdidText {
        let mut bytes = [0u8; 13];
        let mut len = 0usize;
        for (slot, byte) in bytes.iter_mut().zip(raw.iter()) {
            // The descriptor pads with spaces and terminates with a line feed.
            if *byte == b'\n' || *byte == 0x00 {
                break;
            }
            *slot = *byte;
            len += 1;
        }
        while len > 0 && bytes.get(len - 1) == Some(&b' ') {
            len -= 1;
        }
        EdidText {
            bytes,
            len: len as u8,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    pub fn as_str(&self) -> Option<&str> {
        str::from_utf8(self.as_bytes()).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for EdidText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.as_str() {
            Some(text) => write!(f, "{text:?}"),
            None => write!(f, "{:?}", self.as_bytes()),
        }
    }
}

/// Additional white point from a 0xFB descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorPoint {
    /// White point index the sink reports (1..=255, 0 when unused).
    pub index: u8,
    pub white_x: u16,
    pub white_y: u16,
    /// Gamma minus 100, in hundredths; `None` when unset (0xFF).
    pub gamma_hundredths: Option<u16>,
}

/// Aspect ratios a CVT range descriptor says the sink accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CvtAspectRatio {
    Ratio4To3,
    Ratio16To9,
    Ratio16To10,
    Ratio5To4,
    Ratio15To9,
}

impl CvtAspectRatio {
    fn from_bits(bits: u8) -> Option<CvtAspectRatio> {
        match bits {
            0 => Some(CvtAspectRatio::Ratio4To3),
            1 => Some(CvtAspectRatio::Ratio16To9),
            2 => Some(CvtAspectRatio::Ratio16To10),
            3 => Some(CvtAspectRatio::Ratio5To4),
            4 => Some(CvtAspectRatio::Ratio15To9),
            _ => None,
        }
    }
}

/// The CVT half of a display range limits descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvtRangeLimits {
    pub version: u8,
    pub revision: u8,
    /// Largest horizontal addressable pixels the sink supports.
    pub max_horizontal_pixels: u16,
    /// Supported aspect ratio bits: 7 = 4:3, 6 = 16:9, 5 = 16:10, 4 = 5:4,
    /// 3 = 15:9.
    pub supported_aspects: u8,
    pub preferred_aspect: Option<CvtAspectRatio>,
    pub standard_blanking: bool,
    pub reduced_blanking: bool,
    /// Supported scaling bits: 7 = horizontal shrink, 6 = horizontal stretch,
    /// 5 = vertical shrink, 4 = vertical stretch.
    pub scaling: u8,
    pub preferred_refresh_hz: u8,
}

/// Secondary GTF parameters from a 0x02 range limits descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecondaryGtf {
    /// Start frequency in kHz (the descriptor stores 2 kHz units).
    pub start_frequency_khz: u16,
    /// `C`, in half units.
    pub c_half: u8,
    pub m: u16,
    pub k: u8,
    /// `J`, in half units.
    pub j_half: u8,
}

/// Timing formula a range limits descriptor describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeLimitsKind {
    /// 0x00: default GTF, always supported from EDID 1.4 on.
    DefaultGtf,
    /// 0x01: limits only, no timing formula.
    Bare,
    SecondaryGtf(SecondaryGtf),
    Cvt(CvtRangeLimits),
    /// A class byte this parser does not define.
    Reserved(u8),
}

/// A 0xFD display range limits descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeLimits {
    pub min_vertical_hz: u16,
    pub max_vertical_hz: u16,
    pub min_horizontal_khz: u16,
    pub max_horizontal_khz: u16,
    /// Maximum pixel clock in kHz; 0 when the sink does not state one.
    pub max_pixel_clock_khz: u32,
    pub kind: RangeLimitsKind,
}

impl RangeLimits {
    /// True when a timing fits inside the range the sink states.
    ///
    /// This is a range check only: it answers "may this sink be driven at this
    /// rate", not "is this timing correct".  Interlaced modes report their
    /// field rate, which is the rate the sink's limits are stated in.
    pub fn admits(&self, mode: &Mode) -> bool {
        if mode.htotal == 0 || mode.vtotal == 0 {
            return false;
        }
        let refresh = mode.refresh_millihz();
        let line_rate_hz = u64::from(mode.clock_khz) * 1000 / u64::from(mode.htotal);
        let clock_ok = self.max_pixel_clock_khz == 0 || mode.clock_khz <= self.max_pixel_clock_khz;
        clock_ok
            && refresh >= u32::from(self.min_vertical_hz) * 1000
            && refresh <= u32::from(self.max_vertical_hz) * 1000
            && line_rate_hz >= u64::from(self.min_horizontal_khz) * 1000
            && line_rate_hz <= u64::from(self.max_horizontal_khz) * 1000
    }
}

/// Supported refresh rates of one CVT three-byte timing code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvtRefreshSupport {
    pub hz50_standard: bool,
    pub hz60_standard: bool,
    pub hz75_standard: bool,
    pub hz85_standard: bool,
    pub hz60_reduced: bool,
}

/// One CVT three-byte timing code from a 0xF8 descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvtTimingCode {
    /// Addressable lines per field.
    pub lines_per_field: u16,
    /// Aspect ratio bits: 0 = 4:3, 1 = 16:9, 2 = 16:10, 3 = 15:9.
    pub aspect_bits: u8,
    pub refresh: CvtRefreshSupport,
    /// Preferred vertical rate: 0 = 50, 1 = 60, 2 = 75, 3 = 85 Hz.
    pub preferred_refresh_bits: u8,
}

impl CvtTimingCode {
    /// The preferred vertical rate in millihertz.
    pub const fn preferred_refresh_millihz(&self) -> u32 {
        match self.preferred_refresh_bits & 0x3 {
            0 => 50_000,
            1 => 60_000,
            2 => 75_000,
            _ => 85_000,
        }
    }

    /// True when the sink supports reduced blanking at the preferred rate and
    /// does not support standard blanking there.
    pub const fn prefers_reduced_blanking(&self) -> bool {
        matches!(self.preferred_refresh_bits & 0x3, 1)
            && self.refresh.hz60_reduced
            && !self.refresh.hz60_standard
    }

    /// True when the sink supports the preferred rate at all.
    pub const fn supports_preferred_refresh(&self) -> bool {
        match self.preferred_refresh_bits & 0x3 {
            0 => self.refresh.hz50_standard,
            1 => self.refresh.hz60_standard || self.refresh.hz60_reduced,
            2 => self.refresh.hz75_standard,
            _ => self.refresh.hz85_standard,
        }
    }
}

/// A 0xF8 descriptor: a version byte and up to four CVT timing codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvtTimingCodeBlock {
    pub version: u8,
    pub codes: [CvtTimingCode; 4],
    pub len: u8,
}

impl CvtTimingCodeBlock {
    /// The codes the descriptor actually carries.
    pub fn codes(&self) -> &[CvtTimingCode] {
        self.codes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

/// Stereo mode encoded in a detailed timing descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StereoMode {
    None,
    FieldSequentialRight,
    FieldSequentialLeft,
    TwoWayInterleavedRight,
    TwoWayInterleavedLeft,
    FourWayInterleaved,
    SideBySideInterleaved,
}

/// Sync signal a detailed timing descriptor describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncType {
    AnalogComposite,
    BipolarAnalogComposite,
    DigitalComposite,
    /// Digital separate sync: the polarity bits are real polarities, and are
    /// the ones [`Mode`] carries.
    DigitalSeparate,
}

/// A decoded 18-byte detailed timing descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetailedTiming {
    pub mode: Mode,
    pub sync: SyncType,
    pub stereo: StereoMode,
    /// Image size in millimetres, when the descriptor states a real size;
    /// `None` when it encodes an aspect ratio or nothing.
    pub image_size_mm: Option<(u16, u16)>,
    pub border: (u8, u8),
}

/// One of the four 18-byte descriptors of the base block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Descriptor {
    DetailedTiming(DetailedTiming),
    RangeLimits(RangeLimits),
    MonitorName(EdidText),
    MonitorSerial(EdidText),
    UnspecifiedText(EdidText),
    /// 0xFA: six more standard timing identification codes.
    StandardTimingIds([u8; 6]),
    /// 0xFB: additional white point.
    ColorPoint(ColorPoint),
    /// 0xF7: established timings III bitmap.
    EstablishedTimingsIii([u8; 6]),
    /// 0xF8: CVT three-byte timing codes.
    CvtTimingCodes(CvtTimingCodeBlock),
    /// 0x10: a descriptor that carries nothing.
    Dummy,
    /// A sub-tag this parser does not decode, such as colour management data
    /// (0xF9) or a manufacturer-specific code.  The bytes are dropped rather
    /// than guessed at; the sub-tag is kept for the log.
    Undecoded { sub_tag: u8 },
    /// A descriptor that failed to decode.  Only reachable from a lenient
    /// parse; a strict parse returns an [`EdidError`] instead.
    Invalid,
}

/// The base block, fully parsed into owned fields.
#[derive(Clone, Copy, Debug)]
struct BaseBlock {
    version: u8,
    revision: u8,
    vendor: VendorProduct,
    video_input: VideoInput,
    screen_size: ScreenSize,
    gamma_millis: Option<u16>,
    features: Features,
    chromaticity: Chromaticity,
    established: EstablishedTimings,
    standard: [StandardTiming; STANDARD_TIMING_COUNT],
    descriptors: [Descriptor; DESCRIPTOR_COUNT],
    /// True when the sink designates a preferred timing and carries one.
    has_preferred_timing: bool,
}

/// A parsed EDID: the base block plus the caller's buffer for extensions.
#[derive(Clone, Copy)]
pub struct Edid<'a> {
    raw: &'a [u8],
    base: BaseBlock,
    /// Blocks physically present in the buffer, including the base block.
    present_blocks: u8,
    declared_extensions: u8,
    /// Extension blocks that may be iterated: the declared count, capped by
    /// what the buffer actually holds.
    usable_extensions: u8,
    warnings: EdidWarnings,
}

impl fmt::Debug for Edid<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Edid({} {:#06x} v{}.{}, {}+{} blocks, {})",
            self.base.vendor.manufacturer,
            self.base.vendor.product_code,
            self.base.version,
            self.base.revision,
            self.present_blocks,
            self.warnings,
            match self.monitor_name().and_then(EdidText::as_str) {
                Some(name) => name,
                None => "<unnamed>",
            }
        )
    }
}

impl<'a> Edid<'a> {
    /// Parses an EDID strictly.
    ///
    /// Fails on a bad header, a bad checksum anywhere, a missing extension
    /// block the base block declared, an unsupported major version, an
    /// undecodable descriptor or a malformed CTA-861 collection.  Use this
    /// one: a damaged EDID must not silently become a modeset.
    pub fn parse(bytes: &'a [u8]) -> Result<Edid<'a>, EdidError> {
        Edid::parse_with(bytes, ParseOptions::STRICT)
    }

    /// Parses an EDID for bring-up on real hardware.
    ///
    /// The base block must still be structurally valid (header, length,
    /// checksum, version), but a missing extension block, an extension block
    /// with a bad checksum, an undecodable descriptor or a malformed CTA-861
    /// collection is recorded in [`Edid::warnings`] and skipped instead of
    /// failing the whole parse.  The result is never silently partial.
    pub fn parse_lossy(bytes: &'a [u8]) -> Result<Edid<'a>, EdidError> {
        Edid::parse_with(bytes, ParseOptions::LOSSY)
    }

    fn parse_with(bytes: &'a [u8], options: ParseOptions) -> Result<Edid<'a>, EdidError> {
        if bytes.is_empty() {
            return Err(EdidError::Empty);
        }
        if bytes.len() % BLOCK_LEN != 0 {
            return Err(EdidError::NotBlockAligned { len: bytes.len() });
        }
        let present_blocks = bytes.len() / BLOCK_LEN;
        if present_blocks > usize::from(MAX_EXTENSION_BLOCKS) + 1 {
            return Err(EdidError::TooManyBlocks {
                blocks: present_blocks,
            });
        }
        let Some(base_block) = block_at(bytes, 0) else {
            return Err(EdidError::NotBlockAligned { len: bytes.len() });
        };
        if base_block.get(..HEADER.len()) != Some(&HEADER[..]) {
            return Err(EdidError::BadHeader);
        }
        let sum = checksum(base_block);
        if sum != 0 {
            return Err(EdidError::BadBaseChecksum { sum });
        }

        let mut warnings = EdidWarnings::default();
        let declared_extensions = base_block.get(0x7e).copied().unwrap_or(0);
        let present_extensions = (present_blocks - 1) as u8;
        if present_extensions < declared_extensions {
            if options.require_declared_extensions {
                return Err(EdidError::MissingExtensionBlocks {
                    declared: declared_extensions,
                    present: present_extensions,
                });
            }
            warnings.missing_extension_blocks = declared_extensions - present_extensions;
        }
        let usable_extensions = present_extensions.min(declared_extensions);

        let version = base_block.get(0x12).copied().unwrap_or(0);
        let revision = base_block.get(0x13).copied().unwrap_or(0);
        if version != 1 {
            return Err(EdidError::UnsupportedVersion {
                major: version,
                revision,
            });
        }

        // Validate every usable extension block's checksum and collection
        // structure even though the contents are parsed on demand: a caller
        // must not have to walk the extensions to learn that one is corrupt.
        for index in 0..usable_extensions {
            let Some(block) = block_at(bytes, usize::from(index) + 1) else {
                break;
            };
            if checksum(block) != 0 {
                if options.require_valid_extension_checksums {
                    return Err(EdidError::BadExtensionChecksum {
                        block: index + 1,
                        sum: checksum(block),
                    });
                }
                warnings.bad_extension_checksums += 1;
                continue;
            }
            if block.first() == Some(&EXT_TAG_CTA861)
                && let Some(cta) = Extension::parse(block, index + 1).as_cta861()
                && let Err(error) = cta.validate()
            {
                if options.require_valid_cta_blocks {
                    return Err(error);
                }
                warnings.malformed_cta_blocks += 1;
            }
        }

        let base = parse_base(base_block, revision, options, &mut warnings)?;
        Ok(Edid {
            raw: bytes,
            base,
            present_blocks: present_blocks as u8,
            declared_extensions,
            usable_extensions,
            warnings,
        })
    }

    /// The bytes this EDID was parsed from.
    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }

    pub fn warnings(&self) -> EdidWarnings {
        self.warnings
    }

    /// The extension block count the base block declares.
    pub fn declared_extension_blocks(&self) -> u8 {
        self.declared_extensions
    }

    /// The extension block count actually present in the buffer.
    pub fn present_extension_blocks(&self) -> u8 {
        self.present_blocks - 1
    }

    /// `(major, revision)`, for example `(1, 4)`.
    pub fn version(&self) -> (u8, u8) {
        (self.base.version, self.base.revision)
    }

    pub fn vendor(&self) -> VendorProduct {
        self.base.vendor
    }

    pub fn video_input(&self) -> VideoInput {
        self.base.video_input
    }

    pub fn screen_size(&self) -> ScreenSize {
        self.base.screen_size
    }

    /// Gamma as thousandths, or `None` when the block defers it to an
    /// extension (0xFF).
    pub fn gamma_millis(&self) -> Option<u16> {
        self.base.gamma_millis
    }

    pub fn features(&self) -> Features {
        self.base.features
    }

    pub fn chromaticity(&self) -> Chromaticity {
        self.base.chromaticity
    }

    pub fn established_timings(&self) -> EstablishedTimings {
        self.base.established
    }

    pub fn standard_timings(&self) -> &[StandardTiming; STANDARD_TIMING_COUNT] {
        &self.base.standard
    }

    pub fn descriptors(&self) -> &[Descriptor; DESCRIPTOR_COUNT] {
        &self.base.descriptors
    }

    pub fn descriptor(&self, index: usize) -> Option<&Descriptor> {
        self.base.descriptors.get(index)
    }

    /// True when the feature byte designates a preferred timing and the block
    /// actually carries a detailed timing descriptor for it.  From EDID 1.4
    /// on the designation is implied rather than optional.
    pub fn has_preferred_timing(&self) -> bool {
        self.base.has_preferred_timing
    }

    /// The sink's preferred timing: the first detailed timing descriptor.
    pub fn preferred_timing(&self) -> Option<Mode> {
        if !self.base.has_preferred_timing {
            return None;
        }
        self.detailed_timings().next().map(|timing| timing.mode)
    }

    /// Every detailed timing descriptor of the base block, in order.
    pub fn detailed_timings(&self) -> impl Iterator<Item = &DetailedTiming> {
        self.base
            .descriptors
            .iter()
            .filter_map(|descriptor| match descriptor {
                Descriptor::DetailedTiming(timing) => Some(timing),
                _ => None,
            })
    }

    /// Every descriptor of the base block that names a monitor string.
    pub fn monitor_name(&self) -> Option<&EdidText> {
        self.base
            .descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                Descriptor::MonitorName(text) => Some(text),
                _ => None,
            })
    }

    /// The monitor serial number descriptor, if the sink carries one.
    pub fn monitor_serial(&self) -> Option<&EdidText> {
        self.base
            .descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                Descriptor::MonitorSerial(text) => Some(text),
                _ => None,
            })
    }

    /// The display range limits descriptor, if the sink carries one.
    pub fn range_limits(&self) -> Option<&RangeLimits> {
        self.base
            .descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                Descriptor::RangeLimits(limits) => Some(limits),
                _ => None,
            })
    }

    /// The CVT three-byte timing codes of the first 0xF8 descriptor.
    pub fn cvt_timing_codes(&self) -> Option<&CvtTimingCodeBlock> {
        self.base
            .descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                Descriptor::CvtTimingCodes(codes) => Some(codes),
                _ => None,
            })
    }

    /// The six additional standard timing codes of the first 0xFA descriptor.
    pub fn standard_timing_ids(&self) -> Option<[u8; 6]> {
        self.base
            .descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                Descriptor::StandardTimingIds(ids) => Some(*ids),
                _ => None,
            })
    }

    /// Iterates the extension blocks present in the buffer.
    ///
    /// Blocks with a bad checksum are skipped: a strict parse cannot produce
    /// one, and a lenient parse counted it in [`Edid::warnings`].
    pub fn extensions(&self) -> Extensions<'a> {
        Extensions {
            raw: self.raw,
            index: 0,
            count: usize::from(self.usable_extensions),
        }
    }

    /// Iterates the CTA-861 extension blocks, ignoring other kinds.
    pub fn cta_extensions(&self) -> impl Iterator<Item = Cta861<'a>> {
        self.extensions()
            .filter_map(|extension| extension.as_cta861())
    }

    /// The first CTA-861 extension block.
    pub fn first_cta_extension(&self) -> Option<Cta861<'a>> {
        self.cta_extensions().next()
    }
}

/// The 18-byte descriptor tags, for callers that want to talk about them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayDescriptorTag {
    MonitorSerial,
    UnspecifiedText,
    RangeLimits,
    MonitorName,
    ColorPoint,
    StandardTimingIds,
    ColorManagement,
    CvtTimingCodes,
    EstablishedTimingsIii,
    Dummy,
    Other(u8),
}

impl DisplayDescriptorTag {
    fn from_byte(tag: u8) -> DisplayDescriptorTag {
        match tag {
            0xff => DisplayDescriptorTag::MonitorSerial,
            0xfe => DisplayDescriptorTag::UnspecifiedText,
            0xfd => DisplayDescriptorTag::RangeLimits,
            0xfc => DisplayDescriptorTag::MonitorName,
            0xfb => DisplayDescriptorTag::ColorPoint,
            0xfa => DisplayDescriptorTag::StandardTimingIds,
            0xf9 => DisplayDescriptorTag::ColorManagement,
            0xf8 => DisplayDescriptorTag::CvtTimingCodes,
            0xf7 => DisplayDescriptorTag::EstablishedTimingsIii,
            0x10 => DisplayDescriptorTag::Dummy,
            other => DisplayDescriptorTag::Other(other),
        }
    }
}

/// Iterates the extension blocks of an EDID buffer.
#[derive(Clone, Copy, Debug)]
pub struct Extensions<'a> {
    raw: &'a [u8],
    index: usize,
    count: usize,
}

impl<'a> Iterator for Extensions<'a> {
    type Item = Extension<'a>;

    fn next(&mut self) -> Option<Extension<'a>> {
        while self.index < self.count {
            let Some(block) = block_at(self.raw, self.index + 1) else {
                self.index = self.count;
                return None;
            };
            self.index += 1;
            if checksum(block) == 0 {
                return Some(Extension::parse(block, self.index as u8));
            }
        }
        None
    }
}

/// One extension block.
#[derive(Clone, Copy, Debug)]
pub enum Extension<'a> {
    /// A CTA-861 extension of revision 3 or later.
    Cta861(Cta861<'a>),
    /// Any other extension: another standard's block, a vendor block, an
    /// older CTA revision, or one this parser does not decode.  The checksum
    /// was validated; the contents are deliberately not interpreted.
    Unknown(UnknownExtension<'a>),
}

impl<'a> Extension<'a> {
    fn parse(block: &'a [u8], index: u8) -> Extension<'a> {
        match block.first() {
            Some(&EXT_TAG_CTA861) => match block.get(1).copied() {
                Some(revision) if revision >= CTA_REVISION_DATA_BLOCKS => {
                    Extension::Cta861(Cta861 { raw: block, index })
                }
                _ => Extension::Unknown(UnknownExtension { raw: block }),
            },
            _ => Extension::Unknown(UnknownExtension { raw: block }),
        }
    }

    /// The block's tag byte.  For a CTA-861 block this byte is the revision
    /// instead; [`Extension::revision`] names that case explicitly.
    pub fn tag(&self) -> u8 {
        match self {
            Extension::Cta861(cta) => cta.revision(),
            Extension::Unknown(unknown) => unknown.tag(),
        }
    }

    pub fn revision(&self) -> u8 {
        self.tag()
    }

    pub fn as_cta861(&self) -> Option<Cta861<'a>> {
        match self {
            Extension::Cta861(cta) => Some(*cta),
            Extension::Unknown(_) => None,
        }
    }

    pub fn as_unknown(&self) -> Option<UnknownExtension<'a>> {
        match self {
            Extension::Cta861(_) => None,
            Extension::Unknown(unknown) => Some(*unknown),
        }
    }
}

/// An extension block this parser validates but does not interpret.
#[derive(Clone, Copy, Debug)]
pub struct UnknownExtension<'a> {
    raw: &'a [u8],
}

impl<'a> UnknownExtension<'a> {
    /// The extension tag, which for a CTA-861 block this parser declined (an
    /// older revision) is 0x02 and for anything else is the block's first
    /// byte.
    pub fn tag(&self) -> u8 {
        self.raw.first().copied().unwrap_or(0)
    }

    pub fn raw(&self) -> &'a [u8] {
        self.raw
    }
}

/// One data block of a CTA-861 extension.
#[derive(Clone, Copy, Debug)]
pub struct DataBlock<'a> {
    pub tag: DataBlockTag,
    pub payload: &'a [u8],
}

/// CTA-861 data block tags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataBlockTag {
    Audio,
    Video,
    VendorSpecific,
    SpeakerAllocation,
    VesaLowPower,
    Reserved,
    /// Tag 7: the real tag is the payload's first byte.
    Extended(u8),
}

/// Iterates the data block collection of a CTA-861 extension.
#[derive(Clone, Copy, Debug)]
pub struct DataBlocks<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for DataBlocks<'a> {
    type Item = DataBlock<'a>;

    fn next(&mut self) -> Option<DataBlock<'a>> {
        let header = *self.rest.first()?;
        let tag_bits = header >> 5;
        let length = usize::from(header & 0x1f);
        let payload = self.rest.get(1..1 + length)?;
        self.rest = self.rest.get(1 + length..).unwrap_or(&[]);
        let tag = match tag_bits {
            1 => DataBlockTag::Audio,
            2 => DataBlockTag::Video,
            3 => DataBlockTag::VendorSpecific,
            4 => DataBlockTag::SpeakerAllocation,
            5 => DataBlockTag::VesaLowPower,
            6 => DataBlockTag::Reserved,
            _ => DataBlockTag::Extended(payload.first().copied().unwrap_or(0)),
        };
        Some(DataBlock { tag, payload })
    }
}

/// One short video descriptor: a VIC plus the sink's "native" flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortVideoDescriptor {
    pub vic: u8,
    /// Bit 7: the sink considers this one of its native (preferred) formats.
    pub native: bool,
}

/// A CTA-861 extension block.
#[derive(Clone, Copy, Debug)]
pub struct Cta861<'a> {
    raw: &'a [u8],
    /// Which extension block of the EDID this is, for error messages.
    index: u8,
}

impl<'a> Cta861<'a> {
    fn byte(&self, index: usize) -> u8 {
        self.raw.get(index).copied().unwrap_or(0)
    }

    /// Which extension block of the EDID this is, counting the base block as
    /// block 0.
    pub fn block_index(&self) -> u8 {
        self.index
    }

    pub fn revision(&self) -> u8 {
        self.byte(0)
    }

    /// Offset of the detailed timing descriptors, or 0 when the block carries
    /// neither data blocks nor timings.
    pub fn dtd_offset(&self) -> u8 {
        self.byte(1)
    }

    /// Bit 7 of byte 2: the sink supports underscan.
    pub fn underscan(&self) -> bool {
        self.byte(2) & 0x80 != 0
    }

    /// Bit 6 of byte 2: basic audio is supported.
    pub fn basic_audio(&self) -> bool {
        self.byte(2) & 0x40 != 0
    }

    pub fn ycbcr444(&self) -> bool {
        self.byte(2) & 0x20 != 0
    }

    pub fn ycbcr422(&self) -> bool {
        self.byte(2) & 0x10 != 0
    }

    /// Bits 3..0 of byte 2: how many of the extension's detailed timings the
    /// sink considers native, counted from the first.
    pub fn native_dtd_count(&self) -> u8 {
        self.byte(2) & 0x0f
    }

    /// The data block collection starts at byte 4; byte 3 is reserved.
    fn collection(&self) -> &'a [u8] {
        let offset = self.dtd_offset();
        if offset < 4 {
            return &[];
        }
        let end = usize::from(offset).min(CHECKSUM_OFFSET);
        self.raw.get(4..end).unwrap_or(&[])
    }

    /// Iterates the data block collection.
    ///
    /// Iteration stops at the first block whose length runs past the end of
    /// the collection.  [`Cta861::validate`] reports that condition, so a
    /// caller that cares can fail instead of truncating.
    pub fn data_blocks(&self) -> DataBlocks<'a> {
        DataBlocks {
            rest: self.collection(),
        }
    }

    /// Iterates the short video descriptors of every video data block.
    pub fn short_video_descriptors(&self) -> ShortVideoDescriptors<'a> {
        ShortVideoDescriptors {
            blocks: self.data_blocks(),
            rest: &[],
        }
    }

    /// Iterates the detailed timing descriptors of the extension.
    pub fn detailed_timings(&self) -> CtaDetailedTimings<'a> {
        let offset = usize::from(self.dtd_offset());
        let rest = if offset >= 4 && offset < CHECKSUM_OFFSET {
            self.raw.get(offset..CHECKSUM_OFFSET).unwrap_or(&[])
        } else {
            &[]
        };
        CtaDetailedTimings { rest, index: 0 }
    }

    /// Checks that the collections of this block are self-consistent and that
    /// every detailed timing it carries is programmable.
    ///
    /// A CTA-861 extension is the sink's main chance to advertise modes, so a
    /// block whose data blocks run past their end, whose timing area overlaps
    /// them, or which carries an unusable descriptor is an error rather than
    /// something to guess about.
    pub fn validate(&self) -> Result<(), EdidError> {
        let offset = self.dtd_offset();
        if offset == 0 {
            return Ok(());
        }
        if offset < 4 || usize::from(offset) >= CHECKSUM_OFFSET {
            return Err(EdidError::MalformedCtaDetailedTimings { block: self.index });
        }
        let mut rest = self.collection();
        while let Some(header) = rest.first().copied() {
            let length = usize::from(header & 0x1f);
            let Some(next) = rest.get(1 + length..) else {
                return Err(EdidError::MalformedCtaDataBlocks { block: self.index });
            };
            rest = next;
        }
        for (_, descriptor) in self.detailed_timings() {
            if matches!(descriptor, Descriptor::Invalid) {
                return Err(EdidError::MalformedCtaDetailedTimings { block: self.index });
            }
        }
        Ok(())
    }
}

/// Iterates the short video descriptors of a CTA-861 extension.
#[derive(Clone, Copy, Debug)]
pub struct ShortVideoDescriptors<'a> {
    blocks: DataBlocks<'a>,
    rest: &'a [u8],
}

impl<'a> Iterator for ShortVideoDescriptors<'a> {
    type Item = ShortVideoDescriptor;

    fn next(&mut self) -> Option<ShortVideoDescriptor> {
        loop {
            if let Some(&byte) = self.rest.first() {
                self.rest = self.rest.get(1..).unwrap_or(&[]);
                return Some(ShortVideoDescriptor {
                    vic: byte & 0x7f,
                    native: byte & 0x80 != 0,
                });
            }
            let block = self.blocks.next()?;
            if block.tag == DataBlockTag::Video {
                self.rest = block.payload;
            }
        }
    }
}

/// Iterates the detailed timing descriptors of a CTA-861 extension.
#[derive(Clone, Copy, Debug)]
pub struct CtaDetailedTimings<'a> {
    rest: &'a [u8],
    index: u8,
}

impl Iterator for CtaDetailedTimings<'_> {
    type Item = (u8, Descriptor);

    fn next(&mut self) -> Option<(u8, Descriptor)> {
        let slice = self.rest.get(..DESCRIPTOR_LEN)?;
        let chunk: &[u8; DESCRIPTOR_LEN] = slice.try_into().ok()?;
        self.rest = self.rest.get(DESCRIPTOR_LEN..).unwrap_or(&[]);
        let index = self.index;
        self.index = self.index.wrapping_add(1);
        // A zero pixel clock ends the timing list: the rest of the area is
        // padding.  This also bounds the iterator by the block, not by data.
        if chunk.get(..2) == Some(&[0, 0]) {
            return None;
        }
        let descriptor = parse_detailed_timing(chunk, index, TimingSource::CtaDtd { index })
            .map(Descriptor::DetailedTiming)
            .unwrap_or(Descriptor::Invalid);
        Some((index, descriptor))
    }
}

/// Sums a block; a valid block sums to zero modulo 256.
fn checksum(block: &[u8]) -> u8 {
    block.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
}

fn block_at(bytes: &[u8], index: usize) -> Option<&[u8]> {
    bytes.get(index * BLOCK_LEN..(index + 1) * BLOCK_LEN)
}

fn parse_base(
    block: &[u8],
    revision: u8,
    options: ParseOptions,
    warnings: &mut EdidWarnings,
) -> Result<BaseBlock, EdidError> {
    let byte = |index: usize| block.get(index).copied().unwrap_or(0);

    let vendor = VendorProduct {
        manufacturer: PnpId(decode_pnp_id(
            u16::from(byte(0x08)) << 8 | u16::from(byte(0x09)),
        )),
        product_code: u16::from(byte(0x0b)) << 8 | u16::from(byte(0x0a)),
        serial_number: u32::from(byte(0x0f)) << 24
            | u32::from(byte(0x0e)) << 16
            | u32::from(byte(0x0d)) << 8
            | u32::from(byte(0x0c)),
        week: byte(0x10),
        year: u16::from(byte(0x11)) + 1990,
        model_year: byte(0x10) == 0xff,
    };

    let video_input = parse_video_input(byte(0x14));

    let width_cm = byte(0x15);
    let height_cm = byte(0x16);
    let screen_size = if width_cm > 0 && height_cm > 0 {
        ScreenSize {
            width_mm: Some(u16::from(width_cm) * 10),
            height_mm: Some(u16::from(height_cm) * 10),
            landscape_aspect_ratio_percent: None,
            portrait_aspect_ratio_percent: None,
        }
    } else if revision >= 4 {
        ScreenSize {
            width_mm: None,
            height_mm: None,
            landscape_aspect_ratio_percent: (width_cm > 0)
                .then(|| (u16::from(width_cm) + 99) / 100 * 100),
            portrait_aspect_ratio_percent: (height_cm > 0)
                .then(|| (u16::from(height_cm) + 99) / 100 * 100),
        }
    } else {
        ScreenSize {
            width_mm: None,
            height_mm: None,
            landscape_aspect_ratio_percent: None,
            portrait_aspect_ratio_percent: None,
        }
    };

    let gamma_byte = byte(0x17);
    let gamma_millis = (gamma_byte != 0xff).then(|| (u16::from(gamma_byte) + 100) * 10);

    let feature = byte(0x18);
    let is_digital = matches!(video_input, VideoInput::Digital(_));
    let features = Features {
        standby: feature & 0x80 != 0,
        suspend: feature & 0x40 != 0,
        active_off: feature & 0x20 != 0,
        srgb_is_default: feature & 0x04 != 0,
        preferred_timing_is_native: feature & 0x02 != 0,
        continuous_frequency: feature & 0x01 != 0,
        color_bits: (feature >> 3) & 0x3,
        ycbcr444: is_digital && revision >= 4 && feature & 0x08 != 0,
        ycbcr422: is_digital && revision >= 4 && feature & 0x10 != 0,
    };
    let designates_preferred = if revision >= 4 {
        true
    } else {
        feature & 0x02 != 0
    };

    let low = u16::from(byte(0x19));
    let low2 = u16::from(byte(0x1a));
    let chromaticity = Chromaticity {
        red_x: u16::from(byte(0x1b)) << 2 | (low >> 6) & 0x3,
        red_y: u16::from(byte(0x1c)) << 2 | (low >> 4) & 0x3,
        green_x: u16::from(byte(0x1d)) << 2 | (low >> 2) & 0x3,
        green_y: u16::from(byte(0x1e)) << 2 | low & 0x3,
        blue_x: u16::from(byte(0x1f)) << 2 | (low2 >> 6) & 0x3,
        blue_y: u16::from(byte(0x20)) << 2 | (low2 >> 4) & 0x3,
        white_x: u16::from(byte(0x21)) << 2 | (low2 >> 2) & 0x3,
        white_y: u16::from(byte(0x22)) << 2 | low2 & 0x3,
    };

    let mut standard = [StandardTiming::Unused { code: [0x01, 0x01] }; STANDARD_TIMING_COUNT];
    for (index, slot) in standard.iter_mut().enumerate() {
        let offset = 0x26 + index * 2;
        *slot = StandardTiming::parse([byte(offset), byte(offset + 1)]);
    }

    let mut descriptors = [Descriptor::Dummy; DESCRIPTOR_COUNT];
    let mut established_iii = None;
    let mut has_detailed_timing = false;
    for (index, slot) in descriptors.iter_mut().enumerate() {
        let start = DESCRIPTOR_OFFSET + index * DESCRIPTOR_LEN;
        let Some(slice) = block.get(start..start + DESCRIPTOR_LEN) else {
            continue;
        };
        let Ok(chunk) = <&[u8; DESCRIPTOR_LEN]>::try_from(slice) else {
            continue;
        };
        match parse_descriptor(chunk, index as u8, revision) {
            Ok(descriptor) => {
                if let Descriptor::DetailedTiming(_) = descriptor {
                    has_detailed_timing = true;
                }
                if let Descriptor::EstablishedTimingsIii(bits) = descriptor {
                    established_iii = Some(bits);
                }
                *slot = descriptor;
            }
            Err(error) => {
                if options.require_valid_descriptors {
                    return Err(error);
                }
                warnings.invalid_descriptors += 1;
                *slot = Descriptor::Invalid;
            }
        }
    }

    let established = EstablishedTimings {
        i_ii: [byte(0x23), byte(0x24), byte(0x25)],
        iii: established_iii,
    };

    Ok(BaseBlock {
        version: byte(0x12),
        revision,
        vendor,
        video_input,
        screen_size,
        gamma_millis,
        features,
        chromaticity,
        established,
        standard,
        descriptors,
        has_preferred_timing: designates_preferred && has_detailed_timing,
    })
}

fn decode_pnp_id(raw: u16) -> [u8; 3] {
    [
        (((raw >> 10) & 0x1f) as u8).wrapping_add(b'A' - 1),
        (((raw >> 5) & 0x1f) as u8).wrapping_add(b'A' - 1),
        ((raw & 0x1f) as u8).wrapping_add(b'A' - 1),
    ]
}

fn parse_video_input(byte: u8) -> VideoInput {
    if byte & 0x80 != 0 {
        let bit_depth = match (byte >> 4) & 0x7 {
            1 => Some(6),
            2 => Some(8),
            3 => Some(10),
            4 => Some(12),
            5 => Some(14),
            6 => Some(16),
            _ => None,
        };
        let interface = match byte & 0x0f {
            0 => DigitalInterface::Undefined,
            1 => DigitalInterface::Dvi,
            2 => DigitalInterface::HdmiA,
            3 => DigitalInterface::HdmiB,
            4 => DigitalInterface::Mddi,
            5 => DigitalInterface::DisplayPort,
            other => DigitalInterface::Reserved(other),
        };
        VideoInput::Digital(DigitalInput {
            bit_depth,
            interface,
        })
    } else {
        VideoInput::Analog(AnalogInput {
            level: (byte >> 5) & 0x3,
            video_setup: byte & 0x10 != 0,
            separate_sync: byte & 0x08 != 0,
            composite_sync: byte & 0x04 != 0,
            sync_on_green: byte & 0x02 != 0,
            serration: byte & 0x01 != 0,
        })
    }
}

fn parse_descriptor(
    data: &[u8; DESCRIPTOR_LEN],
    index: u8,
    revision: u8,
) -> Result<Descriptor, EdidError> {
    let clock = u16::from(data[0]) | u16::from(data[1]) << 8;
    if clock != 0 {
        return parse_detailed_timing(data, index, TimingSource::EdidDtd { index })
            .map(Descriptor::DetailedTiming);
    }
    // Bytes 0..2 are zero for a display descriptor.  Byte 2 is not checked:
    // some sinks write junk there, and treating a descriptor as "not a timing"
    // is the conservative reading either way.
    let descriptor = match DisplayDescriptorTag::from_byte(data[3]) {
        DisplayDescriptorTag::MonitorSerial => Descriptor::MonitorSerial(EdidText::parse(&data[5..])),
        DisplayDescriptorTag::UnspecifiedText => {
            Descriptor::UnspecifiedText(EdidText::parse(&data[5..]))
        }
        DisplayDescriptorTag::MonitorName => Descriptor::MonitorName(EdidText::parse(&data[5..])),
        DisplayDescriptorTag::RangeLimits => {
            Descriptor::RangeLimits(parse_range_limits(data, revision, index)?)
        }
        DisplayDescriptorTag::StandardTimingIds => {
            let mut ids = [0u8; 6];
            ids.copy_from_slice(&data[5..11]);
            Descriptor::StandardTimingIds(ids)
        }
        DisplayDescriptorTag::ColorPoint => Descriptor::ColorPoint(parse_color_point(data)),
        DisplayDescriptorTag::EstablishedTimingsIii => {
            let mut bits = [0u8; 6];
            bits.copy_from_slice(&data[6..12]);
            Descriptor::EstablishedTimingsIii(bits)
        }
        DisplayDescriptorTag::CvtTimingCodes => Descriptor::CvtTimingCodes(parse_cvt_codes(data)),
        DisplayDescriptorTag::Dummy => Descriptor::Dummy,
        _ => Descriptor::Undecoded { sub_tag: data[3] },
    };
    Ok(descriptor)
}

fn parse_color_point(data: &[u8; DESCRIPTOR_LEN]) -> ColorPoint {
    let gamma = data[13];
    ColorPoint {
        index: data[5],
        white_x: u16::from(data[7]) << 2 | u16::from(data[6] >> 2) & 0x3,
        white_y: u16::from(data[8]) << 2 | u16::from(data[6]) & 0x3,
        gamma_hundredths: (gamma != 0xff).then(|| u16::from(gamma) + 100),
    }
}

fn parse_cvt_codes(data: &[u8; DESCRIPTOR_LEN]) -> CvtTimingCodeBlock {
    let empty = CvtTimingCode {
        lines_per_field: 0,
        aspect_bits: 0,
        refresh: CvtRefreshSupport {
            hz50_standard: false,
            hz60_standard: false,
            hz75_standard: false,
            hz85_standard: false,
            hz60_reduced: false,
        },
        preferred_refresh_bits: 0,
    };
    let mut codes = [empty; 4];
    let mut len = 0u8;
    for (index, slot) in codes.iter_mut().enumerate() {
        let start = 6 + index * 3;
        let code = [data[start], data[start + 1], data[start + 2]];
        if index > 0 && code == [0, 0, 0] {
            break;
        }
        let raw = u16::from(code[0]) | (u16::from(code[1]) & 0xf0) << 4;
        let flags = code[2];
        *slot = CvtTimingCode {
            lines_per_field: (raw + 1) * 2,
            aspect_bits: (code[1] >> 2) & 0x3,
            refresh: CvtRefreshSupport {
                hz50_standard: flags & 0x10 != 0,
                hz60_standard: flags & 0x08 != 0,
                hz75_standard: flags & 0x04 != 0,
                hz85_standard: flags & 0x02 != 0,
                hz60_reduced: flags & 0x01 != 0,
            },
            preferred_refresh_bits: (flags >> 5) & 0x3,
        };
        len += 1;
    }
    CvtTimingCodeBlock {
        version: data[5],
        codes,
        len,
    }
}

/// Splits a value into its low byte and high bits, as every detailed timing
/// field is encoded.
fn split_field(low: u8, high_bits: u8) -> u16 {
    u16::from(low) | (u16::from(high_bits) << 8)
}

fn parse_detailed_timing(
    data: &[u8; DESCRIPTOR_LEN],
    index: u8,
    source: TimingSource,
) -> Result<DetailedTiming, EdidError> {
    let invalid = EdidError::InvalidDetailedTiming { index };
    let clock_10khz = u16::from(data[0]) | u16::from(data[1]) << 8;
    if clock_10khz == 0 {
        return Err(invalid);
    }
    let hdisplay = split_field(data[2], data[4] >> 4);
    let hblank = split_field(data[3], data[4] & 0x0f);
    let vdisplay = split_field(data[5], data[7] >> 4);
    let vblank = split_field(data[6], data[7] & 0x0f);
    let hfront = split_field(data[8], (data[11] >> 6) & 0x3);
    let hsync = split_field(data[9], (data[11] >> 4) & 0x3);
    let vfront = split_field(data[10] >> 4, data[11] & 0x3);
    let vsync = split_field(data[10] & 0x0f, (data[11] >> 2) & 0x3);
    let flags = data[17];

    let sync = match (flags >> 3) & 0x3 {
        0 => SyncType::AnalogComposite,
        1 => SyncType::BipolarAnalogComposite,
        2 => SyncType::DigitalComposite,
        _ => SyncType::DigitalSeparate,
    };
    // Only digital sync has real polarity bits; for an analog or composite
    // signal bit 2 is serration and bit 1 is sync-on-green, so positive sync
    // is programmed rather than guessed.
    let (hsync_positive, vsync_positive) = match sync {
        SyncType::DigitalSeparate | SyncType::DigitalComposite => {
            (flags & 0x02 != 0, flags & 0x04 != 0)
        }
        _ => (true, true),
    };
    let stereo = match ((flags >> 5) & 0x3, flags & 0x1) {
        (0, _) => StereoMode::None,
        (1, 0) => StereoMode::FieldSequentialRight,
        (2, 0) => StereoMode::FieldSequentialLeft,
        (1, 1) => StereoMode::TwoWayInterleavedRight,
        (2, 1) => StereoMode::TwoWayInterleavedLeft,
        (3, 0) => StereoMode::FourWayInterleaved,
        _ => StereoMode::SideBySideInterleaved,
    };

    let width_mm = split_field(data[12], data[14] >> 4);
    let height_mm = split_field(data[13], data[14] & 0x0f);
    // Table 3.21 note 18.2: 16x9 and 4x3 in these two fields state an aspect
    // ratio, not a size.
    let image_size_mm = if width_mm == 0
        || height_mm == 0
        || (width_mm == 16 && height_mm == 9)
        || (width_mm == 4 && height_mm == 3)
    {
        None
    } else {
        Some((width_mm, height_mm))
    };

    let interlaced = flags & 0x80 != 0;
    let (vdisplay, vsync_start, vsync_end, vtotal) = if interlaced
        && INTERLACED_FRAMES.contains(&(hdisplay, vdisplay.saturating_mul(2)))
    {
        // The sink wrote field lines for a frame CTA-861 defines; convert to
        // the frame units this kernel programs, keeping the odd frame total
        // (1080i has 1125 lines, not 1124).
        (
            vdisplay * 2,
            (vdisplay + vfront) * 2,
            (vdisplay + vfront + vsync) * 2,
            (vdisplay + vblank) * 2 | 1,
        )
    } else {
        (
            vdisplay,
            vdisplay + vfront,
            vdisplay + vfront + vsync,
            vdisplay + vblank,
        )
    };

    let mode = Mode::from_fields(
        u32::from(clock_10khz) * 10,
        hdisplay,
        hdisplay + hfront,
        hdisplay + hfront + hsync,
        hdisplay + hblank,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        if interlaced {
            ModeFlags::INTERLACE
        } else {
            ModeFlags::NONE
        },
        source,
    )
    .with_polarity(hsync_positive, vsync_positive);
    if !mode.is_well_formed() {
        return Err(invalid);
    }
    Ok(DetailedTiming {
        mode,
        sync,
        stereo,
        image_size_mm,
        border: (data[15], data[16]),
    })
}

fn parse_range_limits(
    data: &[u8; DESCRIPTOR_LEN],
    revision: u8,
    index: u8,
) -> Result<RangeLimits, EdidError> {
    let invalid = EdidError::InvalidDisplayDescriptor { index };
    let offsets = data[4];
    let mut max_vertical_offset = 0u16;
    let mut min_vertical_offset = 0u16;
    let mut max_horizontal_offset = 0u16;
    let mut min_horizontal_offset = 0u16;
    if revision >= 4 {
        for (shift, max, min) in [
            (0u8, &mut max_vertical_offset, &mut min_vertical_offset),
            (2u8, &mut max_horizontal_offset, &mut min_horizontal_offset),
        ] {
            match (offsets >> shift) & 0x3 {
                0 => {}
                2 => *max = 255,
                3 => {
                    *max = 255;
                    *min = 255;
                }
                _ => return Err(invalid),
            }
        }
    } else if offsets != 0 {
        return Err(invalid);
    }

    let min_vertical_hz = u16::from(data[5]) + min_vertical_offset;
    let max_vertical_hz = u16::from(data[6]) + max_vertical_offset;
    let min_horizontal_khz = u16::from(data[7]) + min_horizontal_offset;
    let max_horizontal_khz = u16::from(data[8]) + max_horizontal_offset;
    if min_vertical_hz == 0
        || max_vertical_hz == 0
        || min_horizontal_khz == 0
        || max_horizontal_khz == 0
        || min_vertical_hz > max_vertical_hz
        || min_horizontal_khz > max_horizontal_khz
    {
        return Err(invalid);
    }
    let mut max_pixel_clock_khz = u32::from(data[9]) * 10_000;

    let kind = match data[10] {
        0x00 => RangeLimitsKind::DefaultGtf,
        0x01 => {
            if revision < 4 {
                return Err(invalid);
            }
            RangeLimitsKind::Bare
        }
        0x02 => RangeLimitsKind::SecondaryGtf(SecondaryGtf {
            start_frequency_khz: u16::from(data[12]) * 2,
            c_half: data[13],
            m: u16::from(data[15]) << 8 | u16::from(data[14]),
            k: data[16],
            j_half: data[17],
        }),
        0x04 => {
            if revision < 4 {
                return Err(invalid);
            }
            // The CVT form refines the maximum clock downwards in 0.25 MHz
            // steps and states the largest horizontal size in 8 pixel units.
            max_pixel_clock_khz =
                max_pixel_clock_khz.saturating_sub(u32::from(data[12] >> 2) * 250);
            let horizontal_8px = u16::from(data[12] & 0x3) << 8 | u16::from(data[13]);
            RangeLimitsKind::Cvt(CvtRangeLimits {
                version: data[11] >> 4,
                revision: data[11] & 0x0f,
                max_horizontal_pixels: horizontal_8px * 8,
                supported_aspects: data[14],
                preferred_aspect: CvtAspectRatio::from_bits(data[15] >> 5),
                standard_blanking: data[15] & 0x08 != 0,
                reduced_blanking: data[15] & 0x10 != 0,
                scaling: data[16],
                preferred_refresh_hz: data[17],
            })
        }
        // A class this parser does not define.  The limits themselves are
        // still usable, so they are kept and the unknown class is reported.
        other => RangeLimitsKind::Reserved(other),
    };

    Ok(RangeLimits {
        min_vertical_hz,
        max_vertical_hz,
        min_horizontal_khz,
        max_horizontal_khz,
        max_pixel_clock_khz,
        kind,
    })
}

/// Parses one 18-byte detailed timing descriptor.  Exposed to the test
/// fixtures so the encoder and the decoder can be checked against each other.
#[cfg(test)]
pub(crate) fn parse_detailed_timing_for_test(data: &[u8; DESCRIPTOR_LEN]) -> DetailedTiming {
    parse_detailed_timing(data, 0, TimingSource::EdidDtd { index: 0 }).expect("valid descriptor")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::modes::{
        dmt,
        fixtures::{
            BaseBlockBuilder, CtaBlockBuilder, DetailedTimingSpec, RangeLimitsSpec, Rng, assemble,
            checksum_byte,
        },
        vic,
    };
    use alloc::{format, vec::Vec};

    /// The block every parser test starts from: a digital 1.4 panel called
    /// "TEST-PANEL" with one preferred 1920x1080@60 descriptor.
    fn panel() -> BaseBlockBuilder {
        BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .product_code(0x1234)
            .serial_number(0xdead_beef)
            .manufacture(20, 2024)
            .screen_size_cm(34, 19)
            .gamma_byte(120)
            .chromaticity([640, 330, 300, 600, 150, 60, 313, 329])
            .detailed_timing(0, &DetailedTimingSpec::new(148_500, 1920, 1080))
    }

    fn parse(bytes: &[u8]) -> Edid<'_> {
        Edid::parse(bytes).expect("the fixture must parse strictly")
    }

    #[test]
    fn base_block_fields_are_parsed_as_encoded() {
        let bytes = panel().build();
        let edid = parse(&bytes);
        let vendor = edid.vendor();
        assert_eq!(vendor.manufacturer.as_str(), "ACR");
        assert_eq!(vendor.manufacturer.as_bytes(), b"ACR");
        assert_eq!(vendor.product_code, 0x1234);
        assert_eq!(vendor.serial_number, 0xdead_beef);
        assert_eq!((vendor.week, vendor.year), (20, 2024));
        assert!(!vendor.model_year);
        assert_eq!(edid.version(), (1, 4));
        assert!(edid.warnings().is_clean());

        match edid.video_input() {
            VideoInput::Digital(input) => {
                assert_eq!(input.bit_depth, Some(8));
                assert_eq!(input.interface, DigitalInterface::DisplayPort);
            }
            VideoInput::Analog(_) => panic!("the fixture declares a digital input"),
        }

        let size = edid.screen_size();
        assert_eq!(size.width_mm, Some(340));
        assert_eq!(size.height_mm, Some(190));
        assert_eq!(edid.gamma_millis(), Some(2200));

        let features = edid.features();
        assert!(features.preferred_timing_is_native);
        assert!(features.continuous_frequency);
        assert!(features.ycbcr444, "bit 3 is YCbCr 4:4:4 for a digital sink");
        assert!(!features.ycbcr422);
        assert!(!features.srgb_is_default);
        assert!(!features.standby && !features.suspend && !features.active_off);

        let chromaticity = edid.chromaticity();
        assert_eq!(chromaticity.red_x, 640);
        assert_eq!(chromaticity.white_y, 329);
        assert_eq!(Chromaticity::permille(640), 625);
        assert!(!chromaticity.primaries_unset());
    }

    #[test]
    fn preferred_detailed_timing_is_programmable() {
        let bytes = panel().build();
        let edid = parse(&bytes);
        assert!(edid.has_preferred_timing());
        let mode = edid.preferred_timing().expect("a preferred timing");
        assert_eq!(mode.clock_khz, 148_500);
        assert_eq!(mode.hdisplay, 1920);
        assert_eq!(mode.vdisplay, 1080);
        assert_eq!(mode.htotal, 2200);
        assert_eq!(mode.vtotal, 1125);
        assert_eq!(mode.hsync_start, 2008);
        assert_eq!(mode.hsync_end, 2052);
        assert_eq!(mode.vsync_start, 1084);
        assert_eq!(mode.vsync_end, 1089);
        assert_eq!(mode.refresh_millihz(), 60_000);
        assert_eq!(mode.source, TimingSource::EdidDtd { index: 0 });
        assert!(mode.is_well_formed());
        assert!(!mode.is_interlaced());
        // The descriptor's image size survives.
        let timing = edid.detailed_timings().next().expect("a timing");
        assert_eq!(timing.image_size_mm, Some((344, 194)));
        assert_eq!(timing.sync, SyncType::DigitalSeparate);
        assert_eq!(timing.stereo, StereoMode::None);
    }

    #[test]
    fn monitor_name_and_range_limits_descriptors_are_parsed() {
        let bytes = panel()
            .range_limits(1, &RangeLimitsSpec::cvt_panel())
            .monitor_name(2, b"TEST-PANEL")
            .build();
        let edid = parse(&bytes);
        let name = edid.monitor_name().expect("a monitor name");
        assert_eq!(name.as_str(), Some("TEST-PANEL"));
        assert_eq!(name.as_bytes(), b"TEST-PANEL");

        let limits = edid.range_limits().expect("a range limits descriptor");
        assert_eq!(limits.min_vertical_hz, 48);
        assert_eq!(limits.max_vertical_hz, 75);
        assert_eq!(limits.min_horizontal_khz, 30);
        assert_eq!(limits.max_horizontal_khz, 90);
        assert_eq!(limits.max_pixel_clock_khz, 170_000);
        match limits.kind {
            RangeLimitsKind::Cvt(cvt) => {
                assert_eq!((cvt.version, cvt.revision), (1, 1));
                assert!(cvt.standard_blanking);
                assert!(cvt.reduced_blanking);
                assert_eq!(cvt.preferred_refresh_hz, 60);
                assert_eq!(cvt.preferred_aspect, Some(CvtAspectRatio::Ratio16To10));
            }
            other => panic!("expected CVT range limits, got {other:?}"),
        }
        // The range admits the fixture's own preferred timing and rejects
        // something far outside it.
        let preferred = edid.preferred_timing().expect("preferred");
        assert!(limits.admits(&preferred));
        let too_fast = Mode {
            clock_khz: 600_000,
            ..preferred
        };
        assert!(!limits.admits(&too_fast));
        let too_slow = Mode {
            clock_khz: 10_000,
            ..preferred
        };
        assert!(!limits.admits(&too_slow));
    }

    #[test]
    fn established_and_standard_timings_are_decoded() {
        // Bit 5 of byte 0x23 is 640x480@60, whose standard timing code is
        // 0x3140 - the "use DMT" escape the standard defines.
        let bytes = panel()
            .established_i_ii([0x20, 0x00, 0x00])
            .standard_timing(1, 640, 0b01, 60)
            .standard_timing_code(2, [0x01, 0x01])
            .standard_timing_code(3, [0x00, 0x55])
            .build();
        let edid = parse(&bytes);
        let established = edid.established_timings();
        assert_eq!(established.i_ii, [0x20, 0x00, 0x00]);
        assert_eq!(established.iii, None);

        let timings = edid.standard_timings();
        assert!(matches!(timings[0], StandardTiming::Unused { .. }));
        match timings[1] {
            StandardTiming::Named {
                code,
                hdisplay,
                vdisplay,
                refresh_hz,
                aspect,
            } => {
                assert_eq!(code, [0x31, 0x40]);
                assert_eq!(hdisplay, 640);
                assert_eq!(vdisplay, 480);
                assert_eq!(refresh_hz, 60);
                assert_eq!(aspect, AspectRatio::Ratio4To3);
                // The escape resolves through the DMT table's own identifier,
                // which must be the row whose blanking the sink means.
                let entry = dmt::by_std_id(u16::from_be_bytes(code)).expect("DMT 640x480@60");
                assert_eq!(entry.code, 0x04);
                assert_eq!(entry.mode.clock_khz, 25_175);
                assert_eq!(entry.mode.htotal, 800);
            }
            other => panic!("expected a named timing, got {other:?}"),
        }
        assert!(matches!(timings[2], StandardTiming::Unused { code } if code == [0x01, 0x01]));
        assert!(matches!(timings[3], StandardTiming::Reserved { code } if code == [0x00, 0x55]));
    }

    #[test]
    fn a_block_without_a_preferred_timing_says_so() {
        // All four descriptors are dummies: the sink advertises nothing.
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"XXX")
            .build();
        let edid = parse(&bytes);
        assert!(!edid.has_preferred_timing());
        assert_eq!(edid.preferred_timing(), None);
        assert_eq!(edid.detailed_timings().count(), 0);
    }

    #[test]
    fn interlaced_detailed_timings_are_converted_to_frame_lines() {
        let spec = DetailedTimingSpec {
            clock_khz: 74_250,
            hdisplay: 1920,
            hblank: 280,
            hfront: 88,
            hsync: 44,
            vdisplay: 540,
            vblank: 22,
            vfront: 2,
            vsync: 5,
            hsync_positive: true,
            vsync_positive: true,
            interlaced: true,
            width_mm: 0,
            height_mm: 0,
            hborder: 0,
            vborder: 0,
        };
        let bytes = BaseBlockBuilder::new().detailed_timing(0, &spec).build();
        let edid = parse(&bytes);
        let mode = edid.preferred_timing().expect("a preferred timing");
        assert!(mode.is_interlaced());
        assert_eq!(mode.vdisplay, 1080);
        assert_eq!(mode.vtotal, 1125);
        assert_eq!(mode.vsync_start, 1084);
        assert_eq!(mode.vsync_end, 1094);
        assert_eq!(mode.refresh_millihz(), 60_000);
        assert_eq!(edid.detailed_timings().next().unwrap().image_size_mm, None);
    }

    #[test]
    fn checksum_and_header_failures_are_rejected() {
        let good = panel().build();
        assert!(Edid::parse(&good).is_ok());

        let bad_checksum = panel().build_with_bad_checksum();
        let error = Edid::parse(&bad_checksum).expect_err("a wrong checksum must be rejected");
        assert!(matches!(error, EdidError::BadBaseChecksum { .. }), "{error:?}");
        // The lenient path cannot rescue a base block: without it there is
        // nothing to trust.
        assert!(Edid::parse_lossy(&bad_checksum).is_err());

        let mut bad_header = good;
        bad_header[0] = 0x01;
        bad_header[CHECKSUM_OFFSET] = 0;
        bad_header[CHECKSUM_OFFSET] = checksum_byte(&bad_header);
        assert!(matches!(
            Edid::parse(&bad_header),
            Err(EdidError::BadHeader)
        ));
    }

    #[test]
    fn truncated_buffers_are_rejected_and_missing_extensions_are_typed() {
        let base = panel().extension_count(1).build();
        // One byte short of a whole block.
        let error = Edid::parse(&base[..BLOCK_LEN - 1]).expect_err("a partial block");
        assert!(matches!(error, EdidError::NotBlockAligned { len } if len == 127));
        assert!(matches!(Edid::parse(&[]), Err(EdidError::Empty)));
        assert!(matches!(
            Edid::parse(&base[..64]),
            Err(EdidError::NotBlockAligned { len: 64 })
        ));
        // The base block claims an extension block that is not in the buffer.
        let error = Edid::parse(&base).expect_err("a missing extension block");
        assert!(
            matches!(
                error,
                EdidError::MissingExtensionBlocks {
                    declared: 1,
                    present: 0
                }
            ),
            "{error:?}"
        );
        // The lenient path accepts it and says what is missing.
        let edid = Edid::parse_lossy(&base).expect("the base block itself is intact");
        assert_eq!(edid.declared_extension_blocks(), 1);
        assert_eq!(edid.present_extension_blocks(), 0);
        assert_eq!(edid.warnings().missing_extension_blocks, 1);
        assert!(!edid.warnings().is_clean());
        assert_eq!(edid.extensions().count(), 0);
        assert_eq!(edid.preferred_timing().map(|m| m.hdisplay), Some(1920));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let bytes = panel().version(2, 0).build();
        let error = Edid::parse(&bytes).expect_err("version 2 is not EDID");
        assert!(matches!(
            error,
            EdidError::UnsupportedVersion {
                major: 2,
                revision: 0
            }
        ));
        assert!(Edid::parse_lossy(&bytes).is_err());
    }

    #[test]
    fn extension_checksums_are_enforced_in_both_directions() {
        let base = panel().extension_count(1).build();
        let good = CtaBlockBuilder::new(3).vics(&[16, 4]).build();
        let bytes = assemble(base, &[good]);
        let edid = parse(&bytes);
        assert_eq!(edid.present_extension_blocks(), 1);
        assert_eq!(edid.extensions().count(), 1);

        let broken = CtaBlockBuilder::new(3).vics(&[16, 4]).build_with_bad_checksum();
        let bytes = assemble(base, &[broken]);
        let error = Edid::parse(&bytes).expect_err("a bad extension checksum must be rejected");
        assert!(
            matches!(error, EdidError::BadExtensionChecksum { block: 1, .. }),
            "{error:?}"
        );
        let edid = Edid::parse_lossy(&bytes).expect("the base block is intact");
        assert_eq!(edid.warnings().bad_extension_checksums, 1);
        assert_eq!(edid.warnings().missing_extension_blocks, 0);
        // The damaged block is skipped rather than half-read.
        assert_eq!(edid.extensions().count(), 0);
        assert_eq!(edid.cta_extensions().count(), 0);
    }

    #[test]
    fn a_cta_extension_is_parsed_into_modes() {
        let base = panel().extension_count(2).build();
        let cta = CtaBlockBuilder::new(3)
            .underscan(true)
            .basic_audio(true)
            .ycbcr444(true)
            .ycbcr422(true)
            .native_dtd_count(1)
            .lpcm_audio()
            .vics(&[16, 4, 97])
            .detailed_timing(&DetailedTimingSpec {
                clock_khz: 241_500,
                hdisplay: 2560,
                hblank: 160,
                hfront: 48,
                hsync: 32,
                vdisplay: 1440,
                vblank: 41,
                vfront: 3,
                vsync: 5,
                hsync_positive: true,
                vsync_positive: false,
                interlaced: false,
                width_mm: 0,
                height_mm: 0,
                hborder: 0,
                vborder: 0,
            })
            .build();
        // An extension this parser does not decode, with a valid checksum.
        let mut displayid = [0u8; BLOCK_LEN];
        displayid[0] = 0x70;
        displayid[1] = 0x13;
        displayid[CHECKSUM_OFFSET] = checksum_byte(&displayid);
        let bytes = assemble(base, &[cta, displayid]);
        let edid = parse(&bytes);

        assert_eq!(edid.warnings(), EdidWarnings::default());
        let cta = edid.first_cta_extension().expect("a CTA-861 extension");
        assert_eq!(cta.revision(), 3);
        assert_eq!(cta.block_index(), 1);
        assert!(cta.underscan() && cta.basic_audio() && cta.ycbcr444() && cta.ycbcr422());
        assert_eq!(cta.native_dtd_count(), 1);

        let blocks: Vec<_> = cta.data_blocks().collect();
        assert_eq!(blocks.len(), 2, "an audio block and a video block");
        assert_eq!(blocks[0].tag, DataBlockTag::Audio);
        assert_eq!(blocks[1].tag, DataBlockTag::Video);

        let svds: Vec<_> = cta.short_video_descriptors().collect();
        assert_eq!(svds.len(), 3);
        assert_eq!(svds[0].vic, 16);
        assert!(!svds[0].native, "the fixture marks no code native");
        assert_eq!(svds[2].vic, 97);

        let timings: Vec<_> = cta.detailed_timings().collect();
        assert_eq!(timings.len(), 1);
        let descriptor = timings[0].1;
        match descriptor {
            Descriptor::DetailedTiming(timing) => {
                assert_eq!(timing.mode.hdisplay, 2560);
                assert_eq!(timing.mode.clock_khz, 241_500);
                assert_eq!(timing.mode.source, TimingSource::CtaDtd { index: 0 });
                assert!(!timing.mode.vsync_positive);
                assert!(timing.mode.is_well_formed());
            }
            other => panic!("expected a detailed timing, got {other:?}"),
        }

        // The second extension is validated and then ignored.
        let extensions: Vec<_> = edid.extensions().collect();
        assert_eq!(extensions.len(), 2);
        assert!(extensions[0].as_cta861().is_some());
        assert!(extensions[1].as_cta861().is_none());
        assert_eq!(extensions[1].tag(), 0x70);
        assert_eq!(
            extensions[1].as_unknown().map(|unknown| unknown.tag()),
            Some(0x70)
        );
    }

    #[test]
    fn native_short_video_descriptors_carry_their_flag() {
        let base = panel().extension_count(1).build();
        let cta = CtaBlockBuilder::new(3).native_vics(&[16]).vics(&[4]).build();
        let bytes = assemble(base, &[cta]);
        let edid = parse(&bytes);
        let svds: Vec<_> = edid
            .first_cta_extension()
            .expect("CTA")
            .short_video_descriptors()
            .collect();
        assert_eq!(svds.len(), 2);
        assert!(svds[0].native);
        assert_eq!(svds[0].vic, 16);
        assert!(!svds[1].native);
    }

    #[test]
    fn a_malformed_cta_data_block_collection_is_rejected() {
        let base = panel().extension_count(1).build();
        let mut cta = [0u8; BLOCK_LEN];
        cta[0] = 3; // revision
        cta[1] = 6; // timings start at 6
        // A video data block claiming 31 bytes of payload in a collection that
        // only has room for one.
        cta[4] = 2 << 5 | 31;
        cta[5] = 16;
        cta[CHECKSUM_OFFSET] = checksum_byte(&cta);
        let bytes = assemble(base, &[cta]);
        let error = Edid::parse(&bytes).expect_err("a truncated data block");
        assert!(
            matches!(error, EdidError::MalformedCtaDataBlocks { block: 1 }),
            "{error:?}"
        );
        let edid = Edid::parse_lossy(&bytes).expect("lenient parse");
        assert_eq!(edid.warnings().malformed_cta_blocks, 1);
        // Enumeration stops at the malformed block instead of running away.
        assert_eq!(
            edid.first_cta_extension()
                .expect("CTA")
                .short_video_descriptors()
                .count(),
            0
        );
    }

    #[test]
    fn a_cta_block_with_no_dtds_reports_no_timings() {
        let base = panel().extension_count(1).build();
        // A CTA block with a data block collection but no detailed timings.
        let cta = CtaBlockBuilder::new(3).vics(&[4]).build();
        let bytes = assemble(base, &[cta]);
        let edid = parse(&bytes);
        let cta = edid.first_cta_extension().expect("CTA");
        assert_eq!(cta.dtd_offset(), 0);
        assert_eq!(cta.detailed_timings().count(), 0);
        assert_eq!(cta.short_video_descriptors().count(), 1);
    }

    #[test]
    fn an_older_cta_revision_is_validated_but_not_decoded() {
        let base = panel().extension_count(1).build();
        let mut old = [0u8; BLOCK_LEN];
        old[0] = EXT_TAG_CTA861;
        old[1] = 1; // revision 1: no data block collection
        old[CHECKSUM_OFFSET] = checksum_byte(&old);
        let bytes = assemble(base, &[old]);
        let edid = parse(&bytes);
        assert_eq!(edid.extensions().count(), 1);
        assert_eq!(edid.cta_extensions().count(), 0);
        let unknown = edid
            .extensions()
            .next()
            .and_then(|extension| extension.as_unknown())
            .expect("an unknown extension");
        assert_eq!(unknown.tag(), EXT_TAG_CTA861);
    }

    #[test]
    fn malformed_detailed_timings_are_rejected_strictly_and_skipped_leniently() {
        // A descriptor whose horizontal total is smaller than its sync end.
        let spec = DetailedTimingSpec {
            clock_khz: 148_500,
            hdisplay: 1920,
            hblank: 10,
            hfront: 88,
            hsync: 44,
            vdisplay: 1080,
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
        };
        let bytes = panel().detailed_timing(1, &spec).build();
        let error = Edid::parse(&bytes).expect_err("an unprogrammable timing");
        assert!(
            matches!(error, EdidError::InvalidDetailedTiming { index: 1 }),
            "{error:?}"
        );
        let edid = Edid::parse_lossy(&bytes).expect("lenient parse");
        assert_eq!(edid.warnings().invalid_descriptors, 1);
        assert!(matches!(edid.descriptor(1), Some(Descriptor::Invalid)));
        // The sink's preferred timing survives.
        assert_eq!(edid.preferred_timing().map(|mode| mode.htotal), Some(2200));
        assert_eq!(edid.detailed_timings().count(), 1);
    }

    #[test]
    fn inconsistent_range_limits_are_rejected() {
        let spec = RangeLimitsSpec {
            min_vertical_hz: 90,
            max_vertical_hz: 50, // reversed
            ..RangeLimitsSpec::gtf()
        };
        let bytes = panel().range_limits(1, &spec).build();
        let error = Edid::parse(&bytes).expect_err("reversed limits");
        assert!(
            matches!(error, EdidError::InvalidDisplayDescriptor { index: 1 }),
            "{error:?}"
        );
        assert_eq!(
            Edid::parse_lossy(&bytes)
                .expect("lenient parse")
                .warnings()
                .invalid_descriptors,
            1
        );
    }

    #[test]
    fn standard_timing_ids_descriptor_is_exposed() {
        let bytes = panel()
            .standard_timing_ids_descriptor(
                1,
                [
                    [0x31, 0x40],
                    [0x45, 0x40],
                    [0x61, 0x40],
                    [0x01, 0x01],
                    [0x01, 0x01],
                    [0x01, 0x01],
                ],
            )
            .build();
        let edid = parse(&bytes);
        assert_eq!(
            edid.standard_timing_ids(),
            Some([0x31, 0x40, 0x45, 0x40, 0x61, 0x40])
        );
        match edid.descriptor(1) {
            Some(Descriptor::StandardTimingIds(ids)) => assert_eq!(ids[0..2], [0x31, 0x40]),
            other => panic!("expected standard timing IDs, got {other:?}"),
        }
    }

    #[test]
    fn established_timings_iii_and_cvt_codes_are_parsed() {
        let bytes = panel()
            .established_timings_iii(1, [0x01, 0x00, 0x00, 0x00, 0x00, 0x00])
            // 1080 lines per field ((raw + 1) * 2 with raw = 539), 16:9
            // aspect (bits 3..2 = 01), preferred rate 60 Hz (bits 6..5 = 01),
            // 60 Hz standard blanking (bit 3) and 60 Hz reduced (bit 0).
            .cvt_timing_codes(2, 1, [[0x1b, 0x24, 0x29], [0, 0, 0], [0, 0, 0], [0, 0, 0]])
            .build();
        let edid = parse(&bytes);
        let established = edid.established_timings();
        assert_eq!(established.iii, Some([0x01, 0, 0, 0, 0, 0]));
        let codes = edid.cvt_timing_codes().expect("CVT codes");
        assert_eq!(codes.version, 1);
        assert_eq!(codes.len, 1);
        let code = codes.codes()[0];
        assert_eq!(code.lines_per_field, 1080);
        assert_eq!(code.aspect_bits, 1);
        assert!(code.refresh.hz60_standard);
        assert!(code.refresh.hz60_reduced);
        assert_eq!(code.preferred_refresh_millihz(), 60_000);
        assert!(code.supports_preferred_refresh());
        // Standard blanking is supported at the preferred rate, so the sink
        // does not need reduced blanking.
        assert!(!code.prefers_reduced_blanking());
    }

    #[test]
    fn a_display_descriptor_with_an_unknown_sub_tag_is_kept_as_opaque() {
        let mut data = [0u8; 18];
        data[3] = 0xf9; // colour management data: not decoded
        data[5] = 0x11;
        let bytes = panel().descriptor(1, data).build();
        let edid = parse(&bytes);
        assert!(matches!(
            edid.descriptor(1),
            Some(Descriptor::Undecoded { sub_tag: 0xf9 })
        ));
    }

    #[test]
    fn an_analog_sink_is_typed_as_analog() {
        // 0x7f: analog, 1.0/0.4 levels, separate sync, composite sync.
        let bytes = panel().analog_input(0x7f).build();
        let edid = parse(&bytes);
        match edid.video_input() {
            VideoInput::Analog(analog) => {
                assert_eq!(analog.level, 3);
                assert!(analog.separate_sync && analog.composite_sync);
                assert!(analog.sync_on_green && analog.serration);
                assert!(analog.video_setup);
            }
            VideoInput::Digital(_) => panic!("expected an analog input"),
        }
        // Feature bits that only mean something for a digital sink stay clear.
        assert!(!edid.features().ycbcr444 && !edid.features().ycbcr422);
    }

    /// The property test: many random-but-structured inputs must never panic,
    /// and anything that parses must stay well formed.
    ///
    /// The mutations are structured on purpose - byte flips inside real
    /// blocks, random extension counts, random truncation - so the inputs
    /// reach the parser's branches instead of being uniformly random noise
    /// that fails at the header.
    #[test]
    fn random_but_structured_edids_never_panic_and_stay_well_formed() {
        let mut rng = Rng::new(0x5eed_1234);
        let mut accepted = 0u32;
        let mut rejected = 0u32;
        let mut modes_seen = 0u32;
        let mut vics_seen = 0u32;
        for iteration in 0..4000 {
            let mut bytes = Vec::new();
            let established = [rng.byte(), rng.byte(), rng.byte()];
            let first_code = [rng.byte(), rng.byte()];
            let second_code = [rng.byte(), rng.byte()];
            let declared_by_the_block = rng.below(4) as u8;
            let mut base = panel()
                .established_i_ii(established)
                .standard_timing_code(0, first_code)
                .standard_timing_code(1, second_code)
                .extension_count(declared_by_the_block);
            if rng.below(2) == 0 {
                base = base.range_limits(1, &RangeLimitsSpec::cvt_panel());
            }
            if rng.below(3) == 0 {
                base = base.detailed_timing(
                    2,
                    &DetailedTimingSpec {
                        clock_khz: (rng.below(600) as u32 + 20) * 1000,
                        hdisplay: (rng.below(4000) + 16) as u16,
                        ..DetailedTimingSpec::new(148_500, 1920, 1080)
                    },
                );
            }
            let mut block = base.build();
            // Flip a few bytes anywhere in the base block.
            for _ in 0..rng.below(4) {
                let index = rng.below(CHECKSUM_OFFSET);
                block[index] ^= 1 << rng.below(8);
            }
            if rng.below(2) == 0 {
                // Sometimes keep the checksum valid, sometimes do not.
                block[CHECKSUM_OFFSET] = 0;
                block[CHECKSUM_OFFSET] = checksum_byte(&block);
            }
            bytes.extend_from_slice(&block);

            let extensions = rng.below(3);
            for _ in 0..extensions {
                let mut extension = [0u8; BLOCK_LEN];
                for byte in extension.iter_mut() {
                    *byte = rng.byte();
                }
                if rng.below(2) == 0 {
                    extension[0] = EXT_TAG_CTA861;
                    extension[1] = 3;
                    extension[2] = rng.byte() & 0x7f;
                    extension[CHECKSUM_OFFSET] = 0;
                    extension[CHECKSUM_OFFSET] = checksum_byte(&extension);
                }
                bytes.extend_from_slice(&extension);
            }
            let declared = bytes[0x7e] as usize;
            let present = bytes.len() / BLOCK_LEN - 1;
            if declared > present && rng.below(2) == 0 {
                // Truncate below what the block declares.
                let keep = 1 + rng.below(present + 1);
                bytes.truncate(keep * BLOCK_LEN);
            }
            if rng.below(8) == 0 {
                bytes.truncate(rng.below(bytes.len() + 1));
            }

            let edid = match Edid::parse(&bytes) {
                Ok(edid) => edid,
                Err(error) => {
                    rejected += 1;
                    // Every error must be printable; a silent failure would be
                    // indistinguishable from a rejected EDID in the log.
                    let text = format!("{error}");
                    assert!(text.len() > 8, "{text}");
                    continue;
                }
            };
            accepted += 1;
            // Every fact a driver would read must be safe to read.
            let _ = format!("{edid:?}");
            let _ = edid.vendor();
            let _ = edid.video_input();
            let _ = edid.screen_size();
            let _ = edid.features();
            let _ = edid.chromaticity();
            let _ = edid.established_timings();
            let _ = edid.monitor_name().and_then(EdidText::as_str);
            let _ = edid.range_limits();
            let _ = edid.cvt_timing_codes();
            let _ = edid.standard_timing_ids();
            for timing in edid.standard_timings() {
                if let StandardTiming::Named {
                    hdisplay,
                    vdisplay,
                    refresh_hz,
                    ..
                } = timing
                {
                    assert!(*hdisplay >= 256, "a named timing has a real width");
                    assert!(*vdisplay > 0);
                    assert!((60..=123).contains(refresh_hz));
                }
            }
            for timing in edid.detailed_timings() {
                assert!(timing.mode.is_well_formed(), "iteration {iteration}");
                assert!(timing.mode.htotal > 0 && timing.mode.vtotal > 0);
                modes_seen += 1;
            }
            if let Some(mode) = edid.preferred_timing() {
                assert!(mode.is_well_formed());
                modes_seen += 1;
            }
            if let Some(limits) = edid.range_limits() {
                // A range check must never panic on an extreme mode.
                let _ = limits.admits(&Mode {
                    clock_khz: u32::MAX,
                    ..edid.preferred_timing().unwrap_or(Mode::from_fields(
                        1,
                        1,
                        1,
                        1,
                        1,
                        1,
                        1,
                        1,
                        1,
                        ModeFlags::NONE,
                        TimingSource::Builtin,
                    ))
                });
            }
            for extension in edid.extensions() {
                if let Some(cta) = extension.as_cta861() {
                    let _ = cta.data_blocks().count();
                    for svd in cta.short_video_descriptors() {
                        assert!(svd.vic <= 127);
                        // Every code a short video descriptor can carry and
                        // that is not zero must resolve in the VIC table.
                        if svd.vic != 0 {
                            assert!(vic::by_vic(svd.vic).is_some(), "VIC {}", svd.vic);
                            vics_seen += 1;
                        }
                    }
                    for (_, descriptor) in cta.detailed_timings() {
                        if let Descriptor::DetailedTiming(timing) = descriptor {
                            assert!(timing.mode.is_well_formed());
                            modes_seen += 1;
                        }
                    }
                }
            }
        }
        // The corpus must actually exercise both outcomes, otherwise this test
        // would pass on a parser that rejects or accepts everything.
        assert!(accepted > 200, "only {accepted} inputs parsed");
        assert!(rejected > 200, "only {rejected} inputs were rejected");
        assert!(modes_seen > 200, "only {modes_seen} modes were produced");
        assert!(vics_seen > 20, "only {vics_seen} short video descriptors");
    }
}
