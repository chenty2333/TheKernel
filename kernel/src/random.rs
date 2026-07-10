use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use chacha20::{
    ChaCha20,
    cipher::{KeyIvInit, StreamCipher},
};
use rand::{Rng, SeedableRng, rngs::SmallRng};
use spin::Mutex;

const ENTROPY_BITS_READY: i32 = 256;
const SECURE_RESEED_INTERVAL: usize = 1024 * 1024;

struct SecureRandomState {
    rng: Option<ChaCha20>,
    generated: usize,
}

static SECURE_RANDOM: Mutex<SecureRandomState> = Mutex::new(SecureRandomState {
    rng: None,
    generated: 0,
});
static INSECURE_RANDOM: Mutex<Option<SmallRng>> = Mutex::new(None);
static INSECURE_SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

fn reseed_secure(state: &mut SecureRandomState) -> AxResult<()> {
    let mut seed = [0u8; 32];
    axdriver::fill_entropy(&mut seed).map_err(|_| AxError::WouldBlock)?;
    let nonce = [0u8; 12];
    state.rng = Some(ChaCha20::new((&seed).into(), (&nonce).into()));
    state.generated = 0;
    Ok(())
}

pub fn fill_secure(buf: &mut [u8]) -> AxResult<()> {
    let mut state = SECURE_RANDOM.lock();
    let mut offset = 0;
    while offset < buf.len() {
        if state.rng.is_none() || state.generated == SECURE_RESEED_INTERVAL {
            reseed_secure(&mut state)?;
        }
        let chunk = (buf.len() - offset).min(SECURE_RESEED_INTERVAL - state.generated);
        buf[offset..offset + chunk].fill(0);
        let Some(rng) = state.rng.as_mut() else {
            return Err(AxError::Io);
        };
        rng.apply_keystream(&mut buf[offset..offset + chunk]);
        state.generated += chunk;
        offset += chunk;
    }
    Ok(())
}

fn insecure_seed() -> [u8; 32] {
    let counter = INSECURE_SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stack_marker = 0u8;
    let words = [
        axhal::time::monotonic_time_nanos(),
        crate::time::wall_time_nanos(),
        counter,
        (&stack_marker as *const u8 as usize as u64).rotate_left(17),
    ];
    let mut seed = [0u8; 32];
    for (dst, word) in seed.chunks_exact_mut(8).zip(words) {
        dst.copy_from_slice(&word.to_ne_bytes());
    }
    seed
}

pub fn fill_insecure(buf: &mut [u8]) {
    if fill_secure(buf).is_ok() {
        return;
    }

    let mut rng = INSECURE_RANDOM.lock();
    rng.get_or_insert_with(|| SmallRng::from_seed(insecure_seed()))
        .fill_bytes(buf);
}

pub fn entropy_bits() -> i32 {
    if axdriver::entropy_source_ready() || SECURE_RANDOM.lock().rng.is_some() {
        ENTROPY_BITS_READY
    } else {
        0
    }
}
