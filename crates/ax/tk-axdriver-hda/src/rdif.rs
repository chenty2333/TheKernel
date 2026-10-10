// SPDX-License-Identifier: Apache-2.0
//! Portable playback capability for the existing HDA analog/HDMI controller.
use axdriver_base::DevError;
use tk_rdif_audio::{
    DriverGeneric, Playback, PlaybackConfig, PlaybackError, PlaybackToken, SampleFormat,
};

use crate::{Controller, Hal, regs::Bus};

const CONFIGURATIONS: [PlaybackConfig; 1] = [PlaybackConfig {
    sample_format: SampleFormat::S16Le,
    sample_rate_hz: 48_000,
    channels: 2,
    period_frames: 1024,
    period_count: 4,
}];

impl<H: Hal + 'static, B: Bus + 'static> DriverGeneric for Controller<H, B> {
    fn name(&self) -> &str {
        "Intel HDA PCM playback"
    }
}

impl<H: Hal + 'static, B: Bus + 'static> Playback for Controller<H, B> {
    fn configurations(&self) -> &[PlaybackConfig] {
        &CONFIGURATIONS
    }

    fn max_poll_interval_ns(&self) -> u64 {
        10_000_000
    }

    fn prepare(&mut self, config: PlaybackConfig) -> Result<(), PlaybackError> {
        if !CONFIGURATIONS.contains(&config) {
            return Err(PlaybackError::Unsupported);
        }
        let period_bytes = config.period_bytes().ok_or(PlaybackError::InvalidPeriod)?;
        let period_bytes = u32::try_from(period_bytes).map_err(|_| PlaybackError::InvalidPeriod)?;
        Controller::prepare(self, period_bytes, u32::from(config.period_count))
            .map_err(playback_error)
    }

    fn submit(&mut self, pcm: &[u8]) -> Result<PlaybackToken, PlaybackError> {
        Controller::submit(self, pcm)
            .map(PlaybackToken)
            .map_err(playback_error)
    }

    fn complete(&mut self) -> Result<Option<PlaybackToken>, PlaybackError> {
        Controller::complete(self)
            .map(|token| token.map(PlaybackToken))
            .map_err(playback_error)
    }

    fn release(&mut self) -> Result<(), PlaybackError> {
        Controller::release(self).map_err(playback_error)
    }

    fn abort(&mut self) -> Result<(), PlaybackError> {
        Controller::abort(self).map_err(playback_error)
    }

    fn shutdown(&mut self) -> Result<(), PlaybackError> {
        Controller::shutdown(self).map_err(playback_error)
    }
}

fn playback_error(error: DevError) -> PlaybackError {
    match error {
        DevError::Unsupported => PlaybackError::Unsupported,
        DevError::InvalidParam => PlaybackError::InvalidPeriod,
        DevError::BadState => PlaybackError::BadState,
        DevError::ResourceBusy | DevError::AlreadyExists => PlaybackError::Busy,
        DevError::Again => PlaybackError::Again,
        DevError::Io | DevError::NoMemory => PlaybackError::Device,
    }
}
