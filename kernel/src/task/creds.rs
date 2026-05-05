use axerrno::{AxError, AxResult};
use axsync::spin::SpinNoIrq;

pub(in crate::task) const CAPABILITY_WORDS: usize = 2;

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::task) struct Credentials {
    ruid: u32,
    euid: u32,
    suid: u32,
    fsuid: u32,
    rgid: u32,
    egid: u32,
    sgid: u32,
    fsgid: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CapabilityState {
    pub(crate) effective: [u32; CAPABILITY_WORDS],
    pub(crate) permitted: [u32; CAPABILITY_WORDS],
    pub(crate) inheritable: [u32; CAPABILITY_WORDS],
    pub(crate) bounding: [u32; CAPABILITY_WORDS],
    pub(crate) securebits: u32,
}

impl CapabilityState {
    const fn full() -> Self {
        Self {
            effective: [u32::MAX; CAPABILITY_WORDS],
            permitted: [u32::MAX; CAPABILITY_WORDS],
            inheritable: [0; CAPABILITY_WORDS],
            bounding: [u32::MAX; CAPABILITY_WORDS],
            securebits: 0,
        }
    }

    fn cap_mask(cap: u32) -> Option<(usize, u32)> {
        let word = cap as usize / u32::BITS as usize;
        (word < CAPABILITY_WORDS).then_some((word, 1_u32 << (cap % u32::BITS)))
    }

    pub(crate) fn has_effective(self, cap: u32) -> bool {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return false;
        };
        self.effective[word] & mask != 0
    }

    pub(crate) fn bounding_contains(self, cap: u32) -> bool {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return false;
        };
        self.bounding[word] & mask != 0
    }

    pub(crate) fn drop_bounding(&mut self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        self.bounding[word] &= !mask;
        Ok(())
    }
}
