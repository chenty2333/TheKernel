//! Bounded receiver ELD validation for the fixed HDMI playback route.
use axdriver_base::{DevError, DevResult};

pub const MAX_SIZE: usize = 84;
const HEADER_SIZE: usize = 4;
const SAD_OFFSET: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Eld {
    bytes: [u8; MAX_SIZE],
    len: u8,
    sad_count: u8,
}

impl Eld {
    pub fn parse(bytes: &[u8]) -> DevResult<Self> {
        if bytes.len() < SAD_OFFSET || bytes.len() > MAX_SIZE {
            return Err(DevError::InvalidParam);
        }
        if (bytes[0] & 0xf8) != 0x10 || (bytes[0] & 0x07) != 0 || (bytes[5] & 0x0c) != 0 {
            return Err(DevError::Unsupported);
        }
        let baseline_len = HEADER_SIZE
            .checked_add(
                usize::from(bytes[2])
                    .checked_mul(4)
                    .ok_or(DevError::InvalidParam)?,
            )
            .ok_or(DevError::InvalidParam)?;
        let monitor_name_len = usize::from(bytes[4] & 0x1f);
        if monitor_name_len > 16 {
            return Err(DevError::InvalidParam);
        }
        let sad_count = usize::from((bytes[5] >> 4) & 0x0f);
        let sad_start = SAD_OFFSET
            .checked_add(monitor_name_len)
            .ok_or(DevError::InvalidParam)?;
        let sad_end = sad_start
            .checked_add(sad_count.checked_mul(3).ok_or(DevError::InvalidParam)?)
            .ok_or(DevError::InvalidParam)?;
        if baseline_len > bytes.len()
            || baseline_len > MAX_SIZE
            || sad_end > baseline_len
            || sad_count == 0
        {
            return Err(DevError::InvalidParam);
        }
        let (sads, remainder) = bytes[sad_start..sad_end].as_chunks::<3>();
        let supports_stereo_s16le_48khz = remainder.is_empty()
            && sads.iter().any(|sad| {
                ((sad[0] >> 3) & 0x0f) == 1
                    && (sad[0] & 0x07) >= 1
                    && sad[1] & (1 << 2) != 0
                    && sad[2] & 1 != 0
            });
        if !supports_stereo_s16le_48khz {
            return Err(DevError::Unsupported);
        }
        let mut copy = [0u8; MAX_SIZE];
        copy[..baseline_len].copy_from_slice(&bytes[..baseline_len]);
        Ok(Self {
            bytes: copy,
            len: baseline_len as u8,
            sad_count: sad_count as u8,
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    pub const fn sad_count(&self) -> u8 {
        self.sad_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_eld() -> [u8; 24] {
        let mut eld = [0; 24];
        eld[0] = 0x10;
        eld[2] = 5;
        eld[4] = 3 << 5;
        eld[5] = 1 << 4;
        eld[20..23].copy_from_slice(&[0x09, 1 << 2, 1]);
        eld
    }

    #[test]
    fn accepts_only_bounded_hdmi_stereo_pcm_eld() {
        let eld = stereo_eld();
        let parsed = Eld::parse(&eld).unwrap();
        assert_eq!(parsed.as_bytes(), &eld[..]);
        assert_eq!(parsed.sad_count(), 1);

        let mut wrong_connection = eld;
        wrong_connection[5] |= 1 << 2;
        assert!(matches!(
            Eld::parse(&wrong_connection),
            Err(DevError::Unsupported)
        ));
        let mut wrong_rate = eld;
        wrong_rate[21] = 1 << 1;
        assert!(matches!(
            Eld::parse(&wrong_rate),
            Err(DevError::Unsupported)
        ));
        let mut wrong_depth = eld;
        wrong_depth[22] = 1 << 1;
        assert!(matches!(
            Eld::parse(&wrong_depth),
            Err(DevError::Unsupported)
        ));
    }

    #[test]
    fn rejects_lengths_that_overrun_the_baseline_or_local_buffer() {
        let mut eld = stereo_eld();
        eld[2] = u8::MAX;
        assert!(matches!(Eld::parse(&eld), Err(DevError::InvalidParam)));
        assert!(matches!(
            Eld::parse(&[0; MAX_SIZE + 1]),
            Err(DevError::InvalidParam)
        ));
    }
}
