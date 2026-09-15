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

/// Linux's `crng_ready()`: whether the secure pool may be used without waiting.
///
/// This probes readiness without consuming bytes.  A zero-length
/// `ReseedingDrbg::fill_bytes` neither reseeds nor reports failure, so it must
/// never be used as the probe.
pub fn is_ready() -> bool {
    axdriver::entropy_source_ready() || SECURE_RANDOM.lock().is_seeded()
}

/// Linux's `wait_for_random_bytes()`: block until the secure pool is ready.
///
/// `getrandom(2)` runs this before it looks at the destination buffer, so a
/// blocking call with `len == 0` waits exactly like a blocking call with a
/// non-empty buffer.
pub fn wait_until_ready() -> AxResult<()> {
    retry_secure_fill(
        false,
        || if is_ready() { Ok(()) } else { Err(AxError::WouldBlock) },
        || {
            crate::task::with_proc_state_hint(crate::task::ProcStateHint::Interruptible, || {
                axtask::future::block_on(axtask::future::interruptible(axtask::future::sleep(
                    core::time::Duration::from_millis(10),
                )))
            })
            .map_err(AxError::from)?
            .map_err(AxError::from)?
            .map_err(AxError::from)
        },
    )
}

/// Wait for initial entropy without holding the DRBG lock across a sleep.
/// The driver interface has no readiness notifier, so retry on a bounded
/// interruptible timer rather than returning EAGAIN for blocking getrandom.
pub fn fill_secure_wait(buf: &mut [u8], nonblocking: bool) -> AxResult<()> {
    retry_secure_fill(
        nonblocking,
        || fill_secure(buf),
        || {
            crate::task::with_proc_state_hint(crate::task::ProcStateHint::Interruptible, || {
                axtask::future::block_on(axtask::future::interruptible(axtask::future::sleep(
                    core::time::Duration::from_millis(10),
                )))
            })
            .map_err(AxError::from)?
            .map_err(AxError::from)?
            .map_err(AxError::from)
        },
    )
}

fn retry_secure_fill(
    nonblocking: bool,
    mut fill: impl FnMut() -> AxResult<()>,
    mut wait: impl FnMut() -> AxResult<()>,
) -> AxResult<()> {
    loop {
        match fill() {
            Err(AxError::WouldBlock) if !nonblocking => wait()?,
            result => return result,
        }
    }
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
    if is_ready() { ENTROPY_BITS_READY } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secure_wait_retries_only_blocking_calls_and_propagates_signals() {
        assert_eq!(
            retry_secure_fill(
                true,
                || Err(AxError::WouldBlock),
                || panic!("nonblocking call slept")
            ),
            Err(AxError::WouldBlock)
        );
        let mut attempts = 0;
        assert_eq!(
            retry_secure_fill(
                false,
                || {
                    attempts += 1;
                    if attempts == 1 {
                        Err(AxError::WouldBlock)
                    } else {
                        Ok(())
                    }
                },
                || Ok(())
            ),
            Ok(())
        );
        assert_eq!(attempts, 2);
        assert_eq!(
            retry_secure_fill(
                false,
                || Err(AxError::WouldBlock),
                || Err(AxError::Interrupted)
            ),
            Err(AxError::Interrupted)
        );
    }
}
