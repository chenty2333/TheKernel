// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_guc_log.c and
// intel_guc_log.h: GuC log sizing and control-level policy.
// Copyright © 2014-2019 Intel Corporation. Full MIT grant: ../LICENSE-MIT.

use alloc::vec::Vec;

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
pub const ACTION_LOG_BUFFER_FILE_FLUSH_COMPLETE: u32 = 0x30;
pub const ACTION_UK_LOG_ENABLE_LOGGING: u32 = 0x40;
pub const ACTION_FORCE_LOG_BUFFER_FLUSH: u32 = 0x302;
pub const GUC_DEBUG_LOG_BUFFER: u32 = 0;
pub const GUC_LOG_CONTROL_LOGGING_ENABLED: u32 = 1;
pub const GUC_LOG_CONTROL_VERBOSITY_SHIFT: u32 = 4;
pub const GUC_LOG_CONTROL_DEFAULT_LOGGING: u32 = 1 << 8;
pub const GUC_LOG_VERBOSITY_MAX: u8 = 3;

/// Build the enable-logging action payload for a GuC log level.
/// upstream: intel_guc_log.c guc_action_control_log()/intel_guc_log_set_level().
pub fn control_log_action(level: u32) -> Result<[u32; 2], Error> {
    if level > u32::from(GUC_LOG_LEVEL_MAX) {
        return Err(Error::Refused);
    }
    let verbosity = if level > u32::from(GUC_LOG_LEVEL_NON_VERBOSE) {
        level - 2
    } else {
        0
    };
    if verbosity > u32::from(GUC_LOG_VERBOSITY_MAX) {
        return Err(Error::Refused);
    }
    let mut control = verbosity << GUC_LOG_CONTROL_VERBOSITY_SHIFT;
    if level > u32::from(GUC_LOG_LEVEL_NON_VERBOSE) {
        control |= GUC_LOG_CONTROL_LOGGING_ENABLED;
    }
    if level > u32::from(GUC_LOG_LEVEL_DISABLED) {
        control |= GUC_LOG_CONTROL_DEFAULT_LOGGING;
    }
    Ok([ACTION_UK_LOG_ENABLE_LOGGING, control])
}

/// Runtime log-level state. The caller serializes access and holds runtime PM
/// while `send` performs the CT request.
pub struct GucLogController {
    level: u8,
}

impl GucLogController {
    pub const fn new(level: u8) -> Self {
        Self { level }
    }

    pub const fn level(&self) -> u8 {
        self.level
    }

    /// Avoid duplicate control messages and commit the new level only after
    /// the synchronous GuC action succeeds.
    /// upstream: intel_guc_log.c intel_guc_log_set_level().
    pub fn set_level(
        &mut self,
        level: u32,
        mut send: impl FnMut([u32; 2]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if level > u32::from(GUC_LOG_LEVEL_MAX) {
            return Err(Error::Refused);
        }
        if self.level == level as u8 {
            return Ok(());
        }
        send(control_log_action(level)?)?;
        self.level = level as u8;
        Ok(())
    }
}

/// CT action sent after the host has consumed a flush-to-file notification.
/// upstream: intel_guc_log.c guc_action_flush_log_complete().
pub const fn flush_log_complete_action() -> [u32; 2] {
    [ACTION_LOG_BUFFER_FILE_FLUSH_COMPLETE, GUC_DEBUG_LOG_BUFFER]
}

/// CT action requesting GuC to flush its current debug log buffer.
/// upstream: intel_guc_log.c guc_action_flush_log().
pub const fn force_log_flush_action() -> [u32; 2] {
    [ACTION_FORCE_LOG_BUFFER_FLUSH, 0]
}

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

/// GuC-owned state at the start of each log buffer section.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct LogBufferState {
    pub marker: [u32; 2],
    pub read_ptr: u32,
    pub write_ptr: u32,
    pub size: u32,
    pub sampled_write_ptr: u32,
    pub wrap_offset: u32,
    /// Bit 0 is flush_to_file, bits 1..4 are the four-bit full counter.
    pub flags: u32,
    pub version: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LogStats {
    pub sampled_overflow: u32,
    pub overflow: u32,
    pub flush: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogBufferType {
    Debug,
    Crash,
    Capture,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogSnapshot {
    /// Header plus debug and crash data, excluding the capture section.
    pub bytes: Vec<u8>,
    pub stats: [LogStats; 3],
}

/// upstream: intel_guc_log.c intel_guc_check_log_buf_overflow().
pub fn intel_guc_check_log_buf_overflow(stats: &mut LogStats, full_count: u32) -> bool {
    let full_count = full_count & 0xf;
    let previous = stats.sampled_overflow;
    if full_count == previous {
        return false;
    }
    stats.overflow = full_count;
    stats.sampled_overflow = stats
        .sampled_overflow
        .wrapping_add(full_count.wrapping_sub(previous));
    if full_count < previous {
        // The hardware counter is four bits wide.
        stats.sampled_overflow = stats.sampled_overflow.wrapping_add(16);
    }
    true
}

/// upstream: intel_guc_log.c intel_guc_get_log_buffer_size().
pub fn intel_guc_get_log_buffer_size(layout: GucLogLayout, kind: LogBufferType) -> u32 {
    match kind {
        LogBufferType::Debug => layout.debug.bytes,
        LogBufferType::Crash => layout.crash.bytes,
        LogBufferType::Capture => layout.capture.bytes,
    }
}

/// upstream: intel_guc_log.c intel_guc_get_log_buffer_offset().
pub fn intel_guc_get_log_buffer_offset(layout: GucLogLayout, kind: LogBufferType) -> u32 {
    match kind {
        LogBufferType::Debug => PAGE_SIZE,
        LogBufferType::Crash => PAGE_SIZE + layout.debug.bytes,
        LogBufferType::Capture => PAGE_SIZE + layout.debug.bytes + layout.crash.bytes,
    }
}

const LOG_STATE_SIZE: usize = 9 * size_of::<u32>();
const LOG_DEBUG_STATE_INDEX: usize = 0;
const LOG_CRASH_STATE_INDEX: usize = 1;

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let word = bytes
        .get(offset..offset.checked_add(4).ok_or(Error::Refused)?)
        .ok_or(Error::Refused)?;
    Ok(u32::from_le_bytes(
        word.try_into().map_err(|_| Error::Refused)?,
    ))
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), Error> {
    let word = bytes
        .get_mut(offset..offset.checked_add(4).ok_or(Error::Refused)?)
        .ok_or(Error::Refused)?;
    word.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// upstream: intel_guc_log.c _guc_log_copy_debuglogs_for_relay().
/// Snapshot the debug and crash rings, advance GuC read pointers, clear flush
/// notifications, and leave capture data out of the relay stream.
pub fn copy_debug_logs_for_relay(
    shared: &mut [u8],
    layout: GucLogLayout,
    stats: &mut [LogStats; 3],
) -> Result<LogSnapshot, Error> {
    let shared_size = usize::try_from(layout.buffer_bytes).map_err(|_| Error::Refused)?;
    let relay_size = shared_size
        .checked_sub(usize::try_from(layout.capture.bytes).map_err(|_| Error::Refused)?)
        .ok_or(Error::Refused)?;
    if shared.len() < shared_size || relay_size < PAGE_SIZE as usize {
        return Err(Error::Refused);
    }
    let mut snapshot = Vec::new();
    snapshot
        .try_reserve_exact(relay_size)
        .map_err(|_| Error::Refused)?;
    snapshot.resize(relay_size, 0);
    let state_bytes = LOG_STATE_SIZE * 2;
    snapshot[..state_bytes].copy_from_slice(&shared[..state_bytes]);

    for (type_index, state_index, kind) in [
        (0usize, LOG_DEBUG_STATE_INDEX, LogBufferType::Debug),
        (1usize, LOG_CRASH_STATE_INDEX, LogBufferType::Crash),
    ] {
        let state_offset = state_index * LOG_STATE_SIZE;
        let read_offset =
            usize::try_from(read_u32(shared, state_offset + 8)?).map_err(|_| Error::Refused)?;
        let sampled_write =
            usize::try_from(read_u32(shared, state_offset + 20)?).map_err(|_| Error::Refused)?;
        let flags = read_u32(shared, state_offset + 28)?;
        let flush_to_file = flags & 1;
        let full_count = (flags >> 1) & 0xf;
        stats[type_index].flush = stats[type_index].flush.wrapping_add(flush_to_file);
        let overflowed = intel_guc_check_log_buf_overflow(&mut stats[type_index], full_count);

        // Keep the upstream shared-memory acknowledgment order: sampled write
        // pointer becomes read pointer, then clear flush_to_file.
        write_u32(
            shared,
            state_offset + 8,
            u32::try_from(sampled_write).map_err(|_| Error::Refused)?,
        )?;
        write_u32(shared, state_offset + 28, flags & !1)?;
        write_u32(
            snapshot.as_mut_slice(),
            state_offset + 12,
            u32::try_from(sampled_write).map_err(|_| Error::Refused)?,
        )?;

        let buffer_size = usize::try_from(intel_guc_get_log_buffer_size(layout, kind))
            .map_err(|_| Error::Refused)?;
        let source = usize::try_from(intel_guc_get_log_buffer_offset(layout, kind))
            .map_err(|_| Error::Refused)?;
        let destination = source;
        let source_end = source.checked_add(buffer_size).ok_or(Error::Refused)?;
        if source_end > shared_size || source_end > relay_size {
            return Err(Error::Refused);
        }
        let (read, write) =
            if overflowed || read_offset > buffer_size || sampled_write > buffer_size {
                (0, buffer_size)
            } else {
                (read_offset, sampled_write)
            };
        if read > write {
            snapshot[destination..destination + write]
                .copy_from_slice(&shared[source..source + write]);
            let tail = buffer_size - read;
            snapshot[destination + read..destination + read + tail]
                .copy_from_slice(&shared[source + read..source + read + tail]);
        } else {
            let count = write - read;
            snapshot[destination + read..destination + read + count]
                .copy_from_slice(&shared[source + read..source + read + count]);
        }
    }

    Ok(LogSnapshot {
        bytes: snapshot,
        stats: *stats,
    })
}

/// Drain the GuC capture region using its own log-state snapshot and preserve
/// the source acknowledgment order: publish read_ptr, then clear flush_to_file.
/// upstream: intel_guc_capture.c __guc_capture_process_output().
pub fn process_capture_log(
    shared: &mut [u8],
    layout: GucLogLayout,
    stats: &mut LogStats,
    reset_in_progress: bool,
) -> Result<crate::guc_capture::CaptureLogResult, Error> {
    let total_size = usize::try_from(layout.buffer_bytes).map_err(|_| Error::Refused)?;
    if shared.len() < total_size {
        return Err(Error::Refused);
    }
    let state_offset = LOG_STATE_SIZE * 2;
    let read_ptr =
        usize::try_from(read_u32(shared, state_offset + 8)?).map_err(|_| Error::Refused)?;
    let sampled_write_ptr =
        usize::try_from(read_u32(shared, state_offset + 20)?).map_err(|_| Error::Refused)?;
    let flags = read_u32(shared, state_offset + 28)?;
    let flush_to_file = flags & 1;
    let full_count = (flags >> 1) & 0xf;
    let capture_offset = usize::try_from(intel_guc_get_log_buffer_offset(
        layout,
        LogBufferType::Capture,
    ))
    .map_err(|_| Error::Refused)?;
    let capture_size = usize::try_from(intel_guc_get_log_buffer_size(
        layout,
        LogBufferType::Capture,
    ))
    .map_err(|_| Error::Refused)?;
    let capture_end = capture_offset
        .checked_add(capture_size)
        .ok_or(Error::Refused)?;
    let data = shared
        .get(capture_offset..capture_end)
        .ok_or(Error::Refused)?;
    let mut state = crate::guc_capture::CaptureLogState {
        read_ptr,
        sampled_write_ptr,
        buffer_full_count: full_count,
        flush_to_file,
    };
    let mut capture_stats = crate::guc_capture::CaptureLogStats {
        sampled_overflow: stats.sampled_overflow,
        overflow: stats.overflow,
        flush: stats.flush,
    };
    let result = crate::guc_capture::process_capture_log(
        data,
        &mut state,
        &mut capture_stats,
        reset_in_progress,
    );
    stats.sampled_overflow = capture_stats.sampled_overflow;
    stats.overflow = capture_stats.overflow;
    stats.flush = capture_stats.flush;
    write_u32(
        shared,
        state_offset + 8,
        u32::try_from(state.read_ptr).map_err(|_| Error::Refused)?,
    )?;
    write_u32(shared, state_offset + 28, flags & !1)?;
    Ok(result)
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
    /// upstream: intel_guc_log.c guc_log_init_sizes() to GuC control fields.
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

    fn tiny_layout() -> GucLogLayout {
        GucLogLayout {
            crash: SectionSize {
                bytes: 8,
                units: 4,
                count: 1,
                flags: 0,
            },
            debug: SectionSize {
                bytes: 16,
                units: 4,
                count: 3,
                flags: 0,
            },
            capture: SectionSize {
                bytes: 4,
                units: 4,
                count: 0,
                flags: 0,
            },
            buffer_bytes: PAGE_SIZE + 16 + 8 + 4,
        }
    }

    #[test]
    fn log_control_payload_and_flush_actions_match_guc_abi() {
        assert_eq!(control_log_action(0), Ok([ACTION_UK_LOG_ENABLE_LOGGING, 0]));
        assert_eq!(
            control_log_action(1),
            Ok([
                ACTION_UK_LOG_ENABLE_LOGGING,
                GUC_LOG_CONTROL_DEFAULT_LOGGING
            ])
        );
        assert_eq!(
            control_log_action(2),
            Ok([
                ACTION_UK_LOG_ENABLE_LOGGING,
                GUC_LOG_CONTROL_LOGGING_ENABLED | GUC_LOG_CONTROL_DEFAULT_LOGGING
            ])
        );
        assert_eq!(
            control_log_action(5),
            Ok([
                ACTION_UK_LOG_ENABLE_LOGGING,
                GUC_LOG_CONTROL_LOGGING_ENABLED
                    | GUC_LOG_CONTROL_DEFAULT_LOGGING
                    | (3 << GUC_LOG_CONTROL_VERBOSITY_SHIFT)
            ])
        );
        assert_eq!(control_log_action(6), Err(Error::Refused));
        assert_eq!(flush_log_complete_action(), [0x30, 0]);
        assert_eq!(force_log_flush_action(), [0x302, 0]);
    }

    #[test]
    fn log_level_change_skips_duplicates_and_commits_only_after_success() {
        let mut controller = GucLogController::new(1);
        let mut sent = Vec::new();
        controller
            .set_level(1, |action| {
                sent.push(action);
                Ok(())
            })
            .unwrap();
        assert!(sent.is_empty());
        assert_eq!(
            controller.set_level(3, |_action| Err(Error::Quarantined)),
            Err(Error::Quarantined)
        );
        assert_eq!(controller.level(), 1);
        controller
            .set_level(3, |action| {
                sent.push(action);
                Ok(())
            })
            .unwrap();
        assert_eq!(controller.level(), 3);
        assert_eq!(sent, [[ACTION_UK_LOG_ENABLE_LOGGING, 0x111]]);
    }

    #[test]
    fn capture_log_drain_uses_capture_state_and_acknowledges_after_parse() {
        let mut layout = tiny_layout();
        layout.capture.bytes = 8;
        layout.buffer_bytes = PAGE_SIZE + layout.debug.bytes + layout.crash.bytes + 8;
        let mut shared = alloc::vec![0; layout.buffer_bytes as usize];
        let state_offset = LOG_STATE_SIZE * 2;
        write_u32(&mut shared, state_offset + 8, 0).unwrap();
        write_u32(&mut shared, state_offset + 20, 8).unwrap();
        write_u32(&mut shared, state_offset + 28, 1).unwrap();
        let capture_offset =
            intel_guc_get_log_buffer_offset(layout, LogBufferType::Capture) as usize;
        write_u32(&mut shared, capture_offset, 0).unwrap();
        write_u32(&mut shared, capture_offset + 4, 0).unwrap();
        let mut stats = LogStats::default();
        let result = process_capture_log(&mut shared, layout, &mut stats, false).unwrap();
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.parse_error, None);
        assert_eq!(result.flush_count, 1);
        assert_eq!(stats.flush, 1);
        assert_eq!(read_u32(&shared, state_offset + 8), Ok(8));
        assert_eq!(read_u32(&shared, state_offset + 28), Ok(0));
    }

    #[test]
    fn release_and_debug_log_sections_match_upstream_units_and_caps() {
        let release = guc_log_init_sizes(false, false).unwrap();
        assert_eq!(core::mem::size_of::<LogBufferState>(), LOG_STATE_SIZE);
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

    #[test]
    fn guc_log_ring_snapshot_acknowledges_wrap_and_excludes_capture() {
        let layout = tiny_layout();
        let mut shared = alloc::vec![0; layout.buffer_bytes as usize];
        write_u32(&mut shared, 8, 14).unwrap();
        write_u32(&mut shared, 20, 4).unwrap();
        write_u32(&mut shared, 28, 1).unwrap();
        write_u32(&mut shared, LOG_STATE_SIZE + 8, 2).unwrap();
        write_u32(&mut shared, LOG_STATE_SIZE + 20, 6).unwrap();
        for index in 0..16 {
            shared[PAGE_SIZE as usize + index] = index as u8;
        }
        for index in 0..8 {
            shared[PAGE_SIZE as usize + 16 + index] = (0x20 + index) as u8;
        }
        let mut stats = [LogStats::default(); 3];
        let snapshot = copy_debug_logs_for_relay(&mut shared, layout, &mut stats).unwrap();
        assert_eq!(snapshot.bytes.len(), PAGE_SIZE as usize + 16 + 8);
        assert_eq!(
            snapshot.bytes[PAGE_SIZE as usize..PAGE_SIZE as usize + 4],
            [0, 1, 2, 3]
        );
        assert_eq!(
            snapshot.bytes[PAGE_SIZE as usize + 14..PAGE_SIZE as usize + 16],
            [14, 15]
        );
        assert_eq!(
            snapshot.bytes[PAGE_SIZE as usize + 16 + 2..PAGE_SIZE as usize + 16 + 6],
            [0x22, 0x23, 0x24, 0x25]
        );
        assert_eq!(read_u32(&shared, 8).unwrap(), 4);
        assert_eq!(read_u32(&shared, 28).unwrap(), 0);
        assert_eq!(read_u32(&snapshot.bytes, 12).unwrap(), 4);
        assert_eq!(stats[0].flush, 1);
    }

    #[test]
    fn guc_log_overflow_counter_extends_four_bit_wrap() {
        let mut stats = LogStats::default();
        assert!(intel_guc_check_log_buf_overflow(&mut stats, 15));
        assert!(intel_guc_check_log_buf_overflow(&mut stats, 0));
        assert_eq!(stats.sampled_overflow, 16);
        assert_eq!(stats.overflow, 0);
        // The upstream helper compares the raw field with sampled_overflow,
        // not overflow; retain that exact accounting behavior.
        assert!(intel_guc_check_log_buf_overflow(&mut stats, 0));
        assert_eq!(stats.sampled_overflow, 16);
    }
}
