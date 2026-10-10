// SPDX-License-Identifier: Apache-2.0
//! Portable PCM playback capability for single-owner, task-context drivers.
//! Adapted from the Apache-2.0 `rdif-audio` interface in `rcore-os/tgoskits`.
#![no_std]

use core::{any::Any, fmt};

/// Minimal identity shared by task-context driver capabilities.
pub trait DriverGeneric: Send + Any {
    /// Human-readable device name.
    fn name(&self) -> &str;

    /// Optional concrete-type access for adapters that support downcasting.
    fn raw_any(&self) -> Option<&dyn Any> {
        None
    }

    /// Optional mutable concrete-type access for adapters that support
    /// downcasting.
    fn raw_any_mut(&mut self) -> Option<&mut dyn Any> {
        None
    }
}

/// The encoding of each interleaved channel sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleFormat {
    /// Signed 16-bit little-endian samples.
    S16Le,
}

/// A complete configuration advertised by a playback device.
///
/// Drivers reject configurations they do not advertise before changing state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackConfig {
    pub sample_format: SampleFormat,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub period_frames: u32,
    pub period_count: u16,
}

impl PlaybackConfig {
    /// Returns the bytes in one period, or `None` for a zero or overflowing
    /// format. `period_count` is checked here as part of validating the whole
    /// advertised stream configuration.
    pub const fn period_bytes(self) -> Option<usize> {
        if self.sample_rate_hz == 0
            || self.channels == 0
            || self.period_frames == 0
            || self.period_count == 0
        {
            return None;
        }
        let sample_bytes = match self.sample_format {
            SampleFormat::S16Le => 2usize,
        };
        match sample_bytes.checked_mul(self.channels as usize) {
            Some(frame_bytes) => frame_bytes.checked_mul(self.period_frames as usize),
            None => None,
        }
    }
}

/// Identifies one accepted submission, unique among outstanding submissions
/// on its device. It is not a global ID or a capability to access DMA memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackToken(pub u16);

/// Portable playback errors, independent of operating-system errors and
/// controller registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackError {
    /// The device does not advertise the requested configuration.
    Unsupported,
    /// The period configuration or submitted period is invalid.
    InvalidPeriod,
    /// The stream is not in a usable state.
    BadState,
    /// Playback must be drained or cancelled first.
    Busy,
    /// No submission capacity is available yet.
    Again,
    /// A device or DMA operation failed.
    Device,
}

impl fmt::Display for PlaybackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "unsupported PCM configuration",
            Self::InvalidPeriod => "invalid PCM period",
            Self::BadState => "playback stream is not in a usable state",
            Self::Busy => "playback must be drained or cancelled first",
            Self::Again => "no submission capacity; collect completions before retrying",
            Self::Device => "playback device or DMA operation failed",
        })
    }
}

/// A registered, movable playback capability. All operations require one
/// owner in task context. No OS locks, scheduling, mixing, conversion, capture,
/// or user ABI are implied.
pub trait Playback: DriverGeneric {
    /// Exact stream configurations supported by this device.
    fn configurations(&self) -> &[PlaybackConfig];

    /// Maximum recommended interval between completion polls while running.
    fn max_poll_interval_ns(&self) -> u64;

    /// Prepare a supported configuration after the prior stream was drained
    /// or explicitly cancelled.
    fn prepare(&mut self, config: PlaybackConfig) -> Result<(), PlaybackError>;

    /// Copy exactly one period before returning success. The caller may then
    /// reuse its input slice. An error accepts no new period. Playback starts
    /// when the first period is accepted; completions preserve submission
    /// order.
    fn submit(&mut self, pcm: &[u8]) -> Result<PlaybackToken, PlaybackError>;

    /// Return each consumed period once, or `None` if none has completed yet.
    fn complete(&mut self) -> Result<Option<PlaybackToken>, PlaybackError>;

    /// Drain the output tail and stop. `Busy` leaves pending work owned by the
    /// device; continue collecting completions or explicitly abort. Completed
    /// but uncollected tokens remain available after release and must be
    /// collected before preparing another stream.
    fn release(&mut self) -> Result<(), PlaybackError>;

    /// Stop and cancel outstanding tokens. On error, resources remain owned or
    /// quarantined; a timeout alone never authorizes release. Re-prepare after
    /// success.
    fn abort(&mut self) -> Result<(), PlaybackError>;

    /// Permanently stop the device. Dropping must also stop or quarantine DMA.
    /// A failed shutdown does not authorize freeing hardware-visible memory.
    fn shutdown(&mut self) -> Result<(), PlaybackError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: PlaybackConfig = PlaybackConfig {
        sample_format: SampleFormat::S16Le,
        sample_rate_hz: 48_000,
        channels: 2,
        period_frames: 1024,
        period_count: 4,
    };

    #[test]
    fn period_size_counts_frames_and_interleaved_channels() {
        assert_eq!(CONFIG.period_bytes(), Some(4096));
        assert_eq!(
            PlaybackConfig {
                channels: 1,
                ..CONFIG
            }
            .period_bytes(),
            Some(2048)
        );
    }

    #[test]
    fn zero_configuration_fields_are_never_valid_periods() {
        for config in [
            PlaybackConfig {
                sample_rate_hz: 0,
                ..CONFIG
            },
            PlaybackConfig {
                channels: 0,
                ..CONFIG
            },
            PlaybackConfig {
                period_frames: 0,
                ..CONFIG
            },
            PlaybackConfig {
                period_count: 0,
                ..CONFIG
            },
        ] {
            assert_eq!(config.period_bytes(), None);
        }
    }
}
