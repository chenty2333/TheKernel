use core::{
    ptr,
    sync::atomic::{AtomicU64, Ordering, compiler_fence},
};

use axerrno::{AxError, AxResult};
use chacha20::{
    ChaCha20,
    cipher::{KeyIvInit, StreamCipher},
};
use spin::Mutex;

const ENTROPY_BITS_READY: i32 = 256;
const SECURE_RESEED_INTERVAL: usize = 1024 * 1024;

struct RandomState {
    ready: bool,
    secure: Option<ChaCha20>,
    insecure: Option<ChaCha20>,
    generated: usize,
}

struct Zeroizing([u8; 32]);

impl Zeroizing {
    const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn as_mut(&mut self) -> &mut [u8; 32] {
        &mut self.0
    }

    fn as_ref(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for Zeroizing {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

static RANDOM: Mutex<RandomState> = Mutex::new(RandomState {
    ready: false,
    secure: None,
    insecure: None,
    generated: 0,
});
static INSECURE_SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

fn insecure_seed() -> Zeroizing {
    let counter = INSECURE_SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stack_marker = 0u8;
    let words = [
        axhal::time::monotonic_time_nanos(),
        crate::time::wall_time_nanos(),
        counter,
        (&stack_marker as *const u8 as usize as u64).rotate_left(17),
    ];
    let mut seed = [0u8; 32];
    for (dst, word) in seed.as_chunks_mut::<8>().0.iter_mut().zip(words) {
        dst.copy_from_slice(&word.to_ne_bytes());
    }
    Zeroizing::new(seed)
}

fn chacha(seed: &[u8; 32]) -> ChaCha20 {
    ChaCha20::new(seed.into(), (&[0u8; 12]).into())
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: each byte belongs to the exclusive stack buffer supplied.
        unsafe { ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// Attempts the one-way first seed without holding the RNG lock during the
/// driver call. A failed later reseed never discards an already-ready key.
pub fn ensure_ready() -> AxResult<()> {
    if RANDOM.lock().ready {
        return Ok(());
    }
    let mut seed = Zeroizing::new([0u8; 32]);
    if axdriver::fill_entropy(seed.as_mut()).is_err() {
        return Err(AxError::WouldBlock);
    }
    let mut state = RANDOM.lock();
    if !state.ready {
        state.secure = Some(chacha(seed.as_ref()));
        state.generated = 0;
        state.ready = true;
    }
    Ok(())
}

/// Returns `WouldBlock` until the CRNG has received its first real seed.
pub fn fill_secure(buf: &mut [u8]) -> AxResult<()> {
    ensure_ready()?;
    let mut state = RANDOM.lock();
    if state.generated >= SECURE_RESEED_INTERVAL
        || state.generated.saturating_add(buf.len()).saturating_add(32) > SECURE_RESEED_INTERVAL
    {
        let mut seed = Zeroizing::new([0u8; 32]);
        if axdriver::fill_entropy(seed.as_mut()).is_ok() {
            state.secure = Some(chacha(seed.as_ref()));
            state.generated = 0;
        }
    }
    // Extract a replacement key from the same stream, then immediately drop
    // the old key. Compromise of the live state cannot reconstruct either
    // this extraction or a later one.
    let mut extract = [0u8; 96];
    let extract_len = buf.len().checked_add(32).ok_or(AxError::InvalidInput)?;
    if extract_len > extract.len() {
        return Err(AxError::InvalidInput);
    }
    let rng = state.secure.as_mut().ok_or(AxError::Io)?;
    rng.apply_keystream(&mut extract[..extract_len]);
    buf.copy_from_slice(&extract[..buf.len()]);
    let mut next_key = [0u8; 32];
    next_key.copy_from_slice(&extract[buf.len()..extract_len]);
    state.secure = Some(chacha(&next_key));
    wipe(&mut next_key);
    wipe(&mut extract[..extract_len]);
    state.generated = state.generated.saturating_add(buf.len());
    Ok(())
}

/// `GRND_INSECURE` uses a persistent ChaCha stream even before readiness.
pub fn fill_insecure(buf: &mut [u8]) -> AxResult<()> {
    let mut state = RANDOM.lock();
    if state.ready {
        drop(state);
        return fill_secure(buf);
    }
    if state.insecure.is_none() {
        let mut seed = insecure_seed();
        state.insecure = Some(chacha(seed.as_ref()));
    }
    let rng = state.insecure.as_mut().expect("insecure RNG initialized");
    buf.fill(0);
    rng.apply_keystream(buf);
    Ok(())
}

pub fn entropy_bits() -> i32 {
    if RANDOM.lock().ready {
        ENTROPY_BITS_READY
    } else {
        0
    }
}
