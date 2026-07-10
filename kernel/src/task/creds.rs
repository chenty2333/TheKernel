use alloc::vec::Vec;

use axerrno::{AxError, AxResult};
use linux_raw_sys::general::CAP_LAST_CAP;

pub(in crate::task) const CAPABILITY_WORDS: usize = 2;

const fn capability_valid_mask_word(word: usize) -> u32 {
    let first_cap = word as u32 * u32::BITS;
    if CAP_LAST_CAP < first_cap {
        return 0;
    }

    let last_bit = CAP_LAST_CAP - first_cap;
    if last_bit >= u32::BITS - 1 {
        u32::MAX
    } else {
        (1u32 << (last_bit + 1)) - 1
    }
}

const CAPABILITY_VALID_MASK: [u32; CAPABILITY_WORDS] =
    [capability_valid_mask_word(0), capability_valid_mask_word(1)];

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Credentials {
    pub(in crate::task) ruid: u32,
    pub(in crate::task) euid: u32,
    pub(in crate::task) suid: u32,
    pub(in crate::task) fsuid: u32,
    pub(in crate::task) rgid: u32,
    pub(in crate::task) egid: u32,
    pub(in crate::task) sgid: u32,
    pub(in crate::task) fsgid: u32,
}

/// The credential fields used by one discretionary-access-control operation.
///
/// This keeps a path operation from repeatedly sampling mutable process state.
/// The backing credential stores are still split until the immutable credential
/// foundation is introduced, so constructing this view is not yet an atomic
/// Linux-style `current_cred()` read.
#[derive(Debug, Clone)]
pub(crate) struct DacCredentialView {
    uid: u32,
    gid: u32,
    supplementary_groups: Vec<u32>,
    effective: [u32; CAPABILITY_WORDS],
}

impl DacCredentialView {
    pub(crate) fn new(
        uid: u32,
        gid: u32,
        supplementary_groups: Vec<u32>,
        effective: [u32; CAPABILITY_WORDS],
    ) -> Self {
        Self {
            uid,
            gid,
            supplementary_groups,
            effective,
        }
    }

    pub(crate) fn uid(&self) -> u32 {
        self.uid
    }

    pub(crate) fn gid(&self) -> u32 {
        self.gid
    }

    pub(crate) fn supplementary_groups(&self) -> &[u32] {
        &self.supplementary_groups
    }

    pub(crate) fn has_capability(&self, cap: u32) -> bool {
        let Some((word, mask)) = CapabilityState::cap_mask(cap) else {
            return false;
        };
        self.effective[word] & mask != 0
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CapabilityState {
    pub(crate) effective: [u32; CAPABILITY_WORDS],
    pub(crate) permitted: [u32; CAPABILITY_WORDS],
    pub(crate) inheritable: [u32; CAPABILITY_WORDS],
    pub(crate) bounding: [u32; CAPABILITY_WORDS],
    pub(crate) ambient: [u32; CAPABILITY_WORDS],
    pub(crate) securebits: u32,
}

impl CapabilityState {
    pub(in crate::task) const fn full() -> Self {
        Self {
            effective: CAPABILITY_VALID_MASK,
            permitted: CAPABILITY_VALID_MASK,
            inheritable: [0; CAPABILITY_WORDS],
            bounding: CAPABILITY_VALID_MASK,
            ambient: [0; CAPABILITY_WORDS],
            securebits: 0,
        }
    }

    pub(in crate::task) fn cap_mask(cap: u32) -> Option<(usize, u32)> {
        if cap > CAP_LAST_CAP {
            return None;
        }
        let word = cap as usize / u32::BITS as usize;
        (word < CAPABILITY_WORDS).then_some((word, 1_u32 << (cap % u32::BITS)))
    }

    pub(crate) fn valid_mask(word: usize) -> u32 {
        CAPABILITY_VALID_MASK[word]
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

    pub(crate) fn ambient_contains(self, cap: u32) -> bool {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return false;
        };
        self.ambient[word] & mask != 0
    }

    pub(crate) fn raise_ambient(&mut self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        self.ambient[word] |= mask;
        Ok(())
    }

    pub(crate) fn lower_ambient(&mut self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        self.ambient[word] &= !mask;
        Ok(())
    }

    pub(crate) fn clear_ambient(&mut self) {
        self.ambient = [0; CAPABILITY_WORDS];
    }

    pub(crate) fn reconcile_ambient(&mut self) {
        for word in 0..CAPABILITY_WORDS {
            self.ambient[word] &= self.permitted[word] & self.inheritable[word];
        }
    }

    pub(crate) fn drop_bounding(&mut self, cap: u32) -> AxResult<()> {
        let Some((word, mask)) = Self::cap_mask(cap) else {
            return Err(AxError::InvalidInput);
        };
        self.bounding[word] &= !mask;
        Ok(())
    }
}
