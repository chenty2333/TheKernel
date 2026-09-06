//! A deterministic ChaCha20 DRBG and an entropy-source driven reseeding wrapper.

#![no_std]

/// Supplies seed material.  Entropy readiness, waiting, and error policy are
/// deliberately owned by the platform implementing this trait.
pub trait EntropySource {
    /// Source-specific failure.
    type Error;
    /// Fills `destination` with entropy.
    fn fill_entropy(&mut self, destination: &mut [u8]) -> Result<(), Self::Error>;
}

/// ChaCha20's seed size in bytes.
pub const SEED_BYTES: usize = 32;
const BLOCK_BYTES: usize = 64;

/// Deterministic ChaCha20 stream generator.
#[derive(Clone)]
pub struct ChaChaDrbg {
    key: [u32; 8],
    counter: u32,
    nonce: [u32; 3],
    block: [u8; BLOCK_BYTES],
    offset: usize,
}

impl ChaChaDrbg {
    /// Initializes a reproducible stream from `seed` with a zero nonce.
    pub fn from_seed(seed: [u8; SEED_BYTES]) -> Self {
        let mut key = [0; 8];
        for (word, bytes) in key.iter_mut().zip(seed.chunks_exact(4)) {
            *word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Self {
            key,
            counter: 0,
            nonce: [0; 3],
            block: [0; BLOCK_BYTES],
            offset: BLOCK_BYTES,
        }
    }
    /// Replaces the key and restarts the stream.
    pub fn reseed(&mut self, seed: [u8; SEED_BYTES]) {
        *self = Self::from_seed(seed);
    }
    /// Fills bytes from the deterministic stream.
    pub fn fill_bytes(&mut self, destination: &mut [u8]) {
        for byte in destination {
            if self.offset == BLOCK_BYTES {
                self.refill();
            }
            *byte = self.block[self.offset];
            self.offset += 1;
        }
    }
    fn refill(&mut self) {
        let mut state = [0u32; 16];
        state[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
        state[4..12].copy_from_slice(&self.key);
        state[12] = self.counter;
        state[13..].copy_from_slice(&self.nonce);
        let initial = state;
        for _ in 0..10 {
            round(&mut state, 0, 4, 8, 12);
            round(&mut state, 1, 5, 9, 13);
            round(&mut state, 2, 6, 10, 14);
            round(&mut state, 3, 7, 11, 15);
            round(&mut state, 0, 5, 10, 15);
            round(&mut state, 1, 6, 11, 12);
            round(&mut state, 2, 7, 8, 13);
            round(&mut state, 3, 4, 9, 14);
        }
        for (index, word) in state.iter_mut().enumerate() {
            *word = word.wrapping_add(initial[index]);
            self.block[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        self.counter = self.counter.wrapping_add(1);
        self.offset = 0;
    }
}
fn round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] ^= state[a];
    state[d] = state[d].rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] ^= state[c];
    state[b] = state[b].rotate_left(7);
}

/// A ChaCha DRBG that obtains a fresh seed after a bounded amount of output.
pub struct ReseedingDrbg<S> {
    source: S,
    drbg: Option<ChaChaDrbg>,
    generated: usize,
    interval: usize,
}
impl<S: EntropySource> ReseedingDrbg<S> {
    /// Creates an unseeded generator. `interval` must be nonzero.
    pub const fn new(source: S, interval: usize) -> Self {
        assert!(interval != 0);
        Self {
            source,
            drbg: None,
            generated: 0,
            interval,
        }
    }
    /// Returns the underlying entropy source.
    pub fn into_source(self) -> S {
        self.source
    }
    /// Reports whether this generator has successfully received seed material.
    ///
    /// This lets a platform expose its entropy-readiness policy without
    /// duplicating the DRBG's state machine.
    pub const fn is_seeded(&self) -> bool {
        self.drbg.is_some()
    }
    /// Fills bytes, reseeding before first use and at each interval boundary.
    pub fn fill_bytes(&mut self, destination: &mut [u8]) -> Result<(), S::Error> {
        let mut offset = 0;
        while offset < destination.len() {
            if self.drbg.is_none() || self.generated == self.interval {
                self.reseed()?;
            }
            let count = (destination.len() - offset).min(self.interval - self.generated);
            self.drbg
                .as_mut()
                .expect("reseed installed a generator")
                .fill_bytes(&mut destination[offset..offset + count]);
            self.generated += count;
            offset += count;
        }
        Ok(())
    }
    /// Immediately replaces the generator seed from the entropy source.
    pub fn reseed(&mut self) -> Result<(), S::Error> {
        let mut seed = [0; SEED_BYTES];
        self.source.fill_entropy(&mut seed)?;
        self.drbg = Some(ChaChaDrbg::from_seed(seed));
        self.generated = 0;
        Ok(())
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    #[test]
    fn deterministic_chacha_vector() {
        let mut d = ChaChaDrbg::from_seed([0; 32]);
        let mut bytes = [0; 16];
        d.fill_bytes(&mut bytes);
        assert_eq!(
            bytes,
            [
                0x76, 0xb8, 0xe0, 0xad, 0xa0, 0xf1, 0x3d, 0x90, 0x40, 0x5d, 0x6a, 0xe5, 0x53, 0x86,
                0xbd, 0x28
            ]
        );
    }
    struct Source {
        seeds: Vec<[u8; 32]>,
        calls: usize,
    }
    impl EntropySource for Source {
        type Error = ();
        fn fill_entropy(&mut self, out: &mut [u8]) -> Result<(), ()> {
            out.copy_from_slice(&self.seeds[self.calls]);
            self.calls += 1;
            Ok(())
        }
    }
    #[test]
    fn reseeds_at_exact_boundary() {
        let source = Source {
            seeds: std::vec![[1; 32], [2; 32]],
            calls: 0,
        };
        let mut d = ReseedingDrbg::new(source, 4);
        let mut out = [0; 8];
        d.fill_bytes(&mut out).unwrap();
        assert_eq!(d.source.calls, 2);
        let mut first = ChaChaDrbg::from_seed([1; 32]);
        let mut second = ChaChaDrbg::from_seed([2; 32]);
        let mut expected = [0; 8];
        first.fill_bytes(&mut expected[..4]);
        second.fill_bytes(&mut expected[4..]);
        assert_eq!(out, expected);
    }
    #[test]
    fn readiness_tracks_the_first_successful_reseed() {
        let source = Source {
            seeds: std::vec![[3; 32]],
            calls: 0,
        };
        let mut d = ReseedingDrbg::new(source, 4);
        assert!(!d.is_seeded());
        d.reseed().unwrap();
        assert!(d.is_seeded());
    }
}
