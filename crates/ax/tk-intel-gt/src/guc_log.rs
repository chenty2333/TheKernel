// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_log.c and
// intel_guc_log.h: GuC log sizing and control-level policy.
// Copyright © 2014-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use crate::{
    Error,
    guc_config::{LogConfig, LogSection},
};

const PAGE_SIZE: u32 = 4096;
const SIZE_4K: u32 = 4096;
const SIZE_1M: u32 = 1024 * 1024;
const GUC_LOG_CRASH_MASK: u32 = 0x3;
const GUC_LOG_DEBUG_MASK: u32 = 0xf;
const GUC_LOG_CAPTURE_MASK: u32 = 0x3;
const GUC_LOG_CRASH_SHIFT: u32 = 4;
const GUC_LOG_DEBUG_SHIFT: u32 = 6;
const GUC_LOG_CAPTURE_SHIFT: u32 = 10;
const GUC_LOG_LOG_ALLOC_UNITS: u32 = 1 << 3;
const GUC_LOG_CAPTURE_ALLOC_UNITS: u32 = 1 << 2;
pub const GUC_LOG_LEVEL_DISABLED: u8 = 0;
pub const GUC_LOG_LEVEL_NON_VERBOSE: u8 = 1;
pub const GUC_LOG_LEVEL_MAX: u8 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SectionSize {
    pub bytes: u32,
    pub units: u32,
    /// Encoded as the number of units minus one, as required by the GuC ABI.
    pub count: u8,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GucLogLayout {
    pub crash: SectionSize,
    pub debug: SectionSize,
    pub capture: SectionSize,
    pub buffer_bytes: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogSectionKind {
    Crash,
    Debug,
    Capture,
}

fn section(bytes: u32, max_count: u32, alloc_flag: u32) -> Result<SectionSize, Error> {
    let (units, flags) = if bytes % SIZE_1M == 0 {
        (SIZE_1M, alloc_flag)
    } else {
        (SIZE_4K, 0)
    };
    if bytes == 0 || bytes % units != 0 {
        return Err(Error::Refused);
    }
    let units_count = bytes / units;
    let count = units_count.saturating_sub(1).min(max_count);
    Ok(SectionSize {
        bytes,
        units,
        count: u8::try_from(count).map_err(|_| Error::Refused)?,
        flags,
    })
}

/// upstream: intel_guc_log.c _guc_log_init_sizes()
pub fn init_sizes(debug_guc: bool, debug_gem: bool) -> Result<GucLogLayout, Error> {
    let (mut crash_bytes, debug_bytes, capture_bytes) = if debug_guc {
        (2 * SIZE_1M, 16 * SIZE_1M, SIZE_1M)
    } else if debug_gem {
        (SIZE_1M, 2 * SIZE_1M, SIZE_1M)
    } else {
        (8 * 1024, 64 * 1024, SIZE_1M)
    };

    if debug_bytes >= SIZE_1M && crash_bytes < SIZE_1M {
        crash_bytes = SIZE_1M;
    }

    let crash = section(crash_bytes, GUC_LOG_CRASH_MASK, GUC_LOG_LOG_ALLOC_UNITS)?;
    let debug = section(debug_bytes, GUC_LOG_DEBUG_MASK, GUC_LOG_LOG_ALLOC_UNITS)?;
    let capture = section(
        capture_bytes,
        GUC_LOG_CAPTURE_MASK,
        GUC_LOG_CAPTURE_ALLOC_UNITS,
    )?;

    let (crash, debug) = if crash.units != debug.units {
        (
            SectionSize {
                units: debug.units,
                count: 0,
                flags: crash.flags,
                ..crash
            },
            debug,
        )
    } else {
        (crash, debug)
    };
    let buffer_bytes = PAGE_SIZE
        .checked_add(crash.bytes)
        .and_then(|value| value.checked_add(debug.bytes))
        .and_then(|value| value.checked_add(capture.bytes))
        .ok_or(Error::Refused)?;
    Ok(GucLogLayout {
        crash,
        debug,
        capture,
        buffer_bytes,
    })
}

/// upstream: intel_guc_log.c guc_log_init_sizes()
pub fn guc_log_init_sizes(debug_guc: bool, debug_gem: bool) -> Result<GucLogLayout, Error> {
    init_sizes(debug_guc, debug_gem)
}

/// upstream: intel_guc_log.c intel_guc_log_section_size_crash()
pub fn intel_guc_log_section_size_crash(layout: GucLogLayout) -> u32 {
    layout.crash.bytes
}

/// upstream: intel_guc_log.c intel_guc_log_section_size_debug()
pub fn intel_guc_log_section_size_debug(layout: GucLogLayout) -> u32 {
    layout.debug.bytes
}

/// upstream: intel_guc_log.c intel_guc_log_section_size_capture()
pub fn intel_guc_log_section_size_capture(layout: GucLogLayout) -> u32 {
    layout.capture.bytes
}

/// upstream: intel_guc_log.c intel_guc_log_size()
pub fn intel_guc_log_size(layout: GucLogLayout) -> u32 {
    layout.buffer_bytes
}

/// upstream: intel_guc_log.c __get_default_log_level()
pub fn default_log_level(debug_guc: bool, debug_gem: bool, requested: i32) -> u8 {
    let debug_default = if debug_guc || debug_gem {
        GUC_LOG_LEVEL_MAX
    } else {
        GUC_LOG_LEVEL_NON_VERBOSE
    };
    if requested < 0 {
        debug_default
    } else if requested > i32::from(GUC_LOG_LEVEL_MAX) {
        if debug_guc || debug_gem {
            GUC_LOG_LEVEL_MAX
        } else {
            GUC_LOG_LEVEL_DISABLED
        }
    } else {
        requested as u8
    }
}

impl GucLogLayout {
    pub fn control_config(self, ggtt_address: u32) -> LogConfig {
        let section = |size: SectionSize| LogSection {
            flags: size.flags,
            count: size.count,
        };
        LogConfig {
            ggtt_address,
            crash: section(self.crash),
            debug: section(self.debug),
            capture: section(self.capture),
        }
    }

    pub fn section_size(self, section: LogSectionKind) -> u32 {
        match section {
            LogSectionKind::Crash => self.crash.bytes,
            LogSectionKind::Debug => self.debug.bytes,
            LogSectionKind::Capture => self.capture.bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_debug_log_sections_match_upstream_units_and_caps() {
        let release = guc_log_init_sizes(false, false).unwrap();
        assert_eq!(release.crash.bytes, 8 * 1024);
        assert_eq!((release.crash.units, release.crash.count), (SIZE_4K, 1));
        assert_eq!(release.debug.bytes, 64 * 1024);
        assert_eq!((release.debug.units, release.debug.count), (SIZE_4K, 15));
        assert_eq!(release.capture.bytes, SIZE_1M);
        assert_eq!(release.capture.flags, GUC_LOG_CAPTURE_ALLOC_UNITS);
        assert_eq!(
            release.buffer_bytes,
            PAGE_SIZE + 8 * 1024 + 64 * 1024 + SIZE_1M
        );

        let debug = guc_log_init_sizes(false, true).unwrap();
        assert_eq!((debug.crash.units, debug.crash.count), (SIZE_1M, 0));
        assert_eq!((debug.debug.units, debug.debug.count), (SIZE_1M, 1));
        assert_eq!(debug.crash.flags, GUC_LOG_LOG_ALLOC_UNITS);
    }

    #[test]
    fn debug_guc_log_sections_keep_maximum_unit_counts() {
        let layout = guc_log_init_sizes(true, false).unwrap();
        assert_eq!((layout.crash.units, layout.crash.count), (SIZE_1M, 1));
        assert_eq!((layout.debug.units, layout.debug.count), (SIZE_1M, 15));
        assert_eq!(layout.capture.count, 0);
        assert_eq!(intel_guc_log_size(layout), PAGE_SIZE + 19 * SIZE_1M);
    }

    #[test]
    fn log_level_defaults_and_out_of_range_fallback_match_upstream() {
        assert_eq!(
            default_log_level(false, false, -1),
            GUC_LOG_LEVEL_NON_VERBOSE
        );
        assert_eq!(default_log_level(true, false, -1), GUC_LOG_LEVEL_MAX);
        assert_eq!(default_log_level(false, false, 6), GUC_LOG_LEVEL_DISABLED);
        assert_eq!(default_log_level(false, true, 6), GUC_LOG_LEVEL_MAX);
        assert_eq!(default_log_level(false, false, 3), 3);
    }
}
