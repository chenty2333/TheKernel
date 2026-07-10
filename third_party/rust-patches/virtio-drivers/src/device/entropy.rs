//! Driver for VirtIO entropy source devices.

use super::common::Feature;
use crate::{Error, Result, hal::Hal, queue::VirtQueue, transport::Transport};

const QUEUE: u16 = 0;
const QUEUE_SIZE: usize = 8;

/// A VirtIO entropy source as defined by the VirtIO RNG device specification.
pub struct VirtIOEntropy<H: Hal, T: Transport> {
    transport: T,
    queue: VirtQueue<H, QUEUE_SIZE>,
}

impl<H: Hal, T: Transport> VirtIOEntropy<H, T> {
    /// Creates and initializes an entropy source device.
    pub fn new(mut transport: T) -> Result<Self> {
        let _ = transport.begin_init(Feature::empty());
        let queue = VirtQueue::new(&mut transport, QUEUE, false, false)?;
        transport.finish_init();
        Ok(Self { transport, queue })
    }

    /// Fills `buf` with bytes supplied by the entropy source.
    pub fn fill_bytes(&mut self, buf: &mut [u8]) -> Result {
        let mut filled = 0;
        while filled < buf.len() {
            let remaining = &mut buf[filled..];
            let used_len =
                self.queue
                    .add_notify_wait_pop(&[], &mut [remaining], &mut self.transport)?
                    as usize;
            if used_len == 0 || used_len > buf.len() - filled {
                return Err(Error::IoError);
            }
            filled += used_len;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, vec};
    use core::ptr::NonNull;
    use std::{sync::Mutex, thread};

    use super::*;
    use crate::{
        hal::fake::FakeHal,
        transport::{
            DeviceType,
            fake::{FakeTransport, QueueStatus, State},
        },
    };

    fn device(
        state: Arc<Mutex<State>>,
        config_space: &mut (),
    ) -> VirtIOEntropy<FakeHal, FakeTransport<()>> {
        VirtIOEntropy::new(FakeTransport {
            device_type: DeviceType::EntropySource,
            max_queue_size: QUEUE_SIZE as u32,
            device_features: 0,
            config_space: NonNull::from(config_space),
            state,
        })
        .unwrap()
    }

    #[test]
    fn retries_short_completions() {
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut config_space = ();
        let mut entropy = device(state.clone(), &mut config_space);
        let handle = thread::spawn(move || {
            for bytes in [&[1, 2][..], &[3, 4, 5][..]] {
                State::wait_until_queue_notified(&state, QUEUE);
                state
                    .lock()
                    .unwrap()
                    .write_to_queue::<QUEUE_SIZE>(QUEUE, bytes);
            }
        });

        let mut buf = [0; 5];
        entropy.fill_bytes(&mut buf).unwrap();
        handle.join().unwrap();

        assert_eq!(buf, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn rejects_zero_length_completion() {
        let state = Arc::new(Mutex::new(State {
            queues: vec![QueueStatus::default()],
            ..Default::default()
        }));
        let mut config_space = ();
        let mut entropy = device(state.clone(), &mut config_space);
        let handle = thread::spawn(move || {
            State::wait_until_queue_notified(&state, QUEUE);
            state
                .lock()
                .unwrap()
                .write_to_queue::<QUEUE_SIZE>(QUEUE, &[]);
        });

        assert_eq!(entropy.fill_bytes(&mut [0; 4]), Err(Error::IoError));
        handle.join().unwrap();
    }
}
