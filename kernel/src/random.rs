use core::sync::atomic::{AtomicU64, Ordering};

use axerrno::{AxError, AxResult};
use axrandom::{ChaChaDrbg, EntropySource, ReseedingDrbg};
use spin::Mutex;

const ENTROPY_BITS_READY: i32 = 256;
const SECURE_RESEED_INTERVAL: usize = 1024 * 1024;

struct KernelEntropySource;

impl EntropySource for KernelEntropySource {
    type Error = ();

    fn fill_entropy(&mut self, destination: &mut [u8]) -> Result<(), Self::Error> {
        axdriver::fill_entropy(destination).map_err(|_| ())
    }
}

static SECURE_RANDOM: Mutex<ReseedingDrbg<KernelEntropySource>> = Mutex::new(ReseedingDrbg::new(
    KernelEntropySource,
    SECURE_RESEED_INTERVAL,
));
static INSECURE_RANDOM: Mutex<Option<ChaChaDrbg>> = Mutex::new(None);
static INSECURE_SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn fill_secure(buf: &mut [u8]) -> AxResult<()> {
    SECURE_RANDOM
        .lock()
        .fill_bytes(buf)
        .map_err(|()| AxError::WouldBlock)
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
    for (dst, word) in seed.as_chunks_mut::<8>().0.iter_mut().zip(words) {
        dst.copy_from_slice(&word.to_ne_bytes());
    }
    seed
}

pub fn fill_insecure(buf: &mut [u8]) {
    if fill_secure(buf).is_ok() {
        return;
    }

    let mut rng = INSECURE_RANDOM.lock();
    rng.get_or_insert_with(|| ChaChaDrbg::from_seed(insecure_seed()))
        .fill_bytes(buf);
}

pub fn entropy_bits() -> i32 {
    if axdriver::entropy_source_ready() || SECURE_RANDOM.lock().is_seeded() {
        ENTROPY_BITS_READY
    } else {
        0
    }
}
