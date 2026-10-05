//! Observe already-published control timers without dispatching or consuming.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    Inactive,
    Retransmit,
    KeepAlive,
    TimeWait,
    ZeroWindowProbe,
    DelayedAck,
}

#[derive(Clone, Copy, Debug)]
pub struct TimerObservation {
    pub kind: TimerKind,
    pub remaining: Duration,
    pub retransmit_timeouts: u32,
    pub probes_sent: u32,
    pub retransmit_delay: Duration,
    pub ack_delay: Duration,
}

fn remaining(deadline: Instant, now: Instant) -> Duration {
    if deadline <= now {
        Duration::ZERO
    } else {
        deadline - now
    }
}
impl Socket<'_> {
    pub fn timer_observation(&self, now: Instant) -> TimerObservation {
        let (kind, delay) = match self.timer {
            Timer::Retransmit { expires_at } => (TimerKind::Retransmit, remaining(expires_at, now)),
            Timer::FastRetransmit => (TimerKind::Retransmit, Duration::ZERO),
            Timer::ZeroWindowProbe { expires_at, .. } => {
                (TimerKind::ZeroWindowProbe, remaining(expires_at, now))
            }
            Timer::Close { expires_at } => (TimerKind::TimeWait, remaining(expires_at, now)),
            Timer::Idle {
                keep_alive_at: Some(deadline),
            } => (TimerKind::KeepAlive, remaining(deadline, now)),
            Timer::Idle {
                keep_alive_at: None,
            } => match self.ack_delay_timer {
                AckDelayTimer::Waiting(deadline) => {
                    (TimerKind::DelayedAck, remaining(deadline, now))
                }
                AckDelayTimer::Immediate => (TimerKind::DelayedAck, Duration::ZERO),
                AckDelayTimer::Idle => (TimerKind::Inactive, Duration::ZERO),
            },
        };
        TimerObservation {
            kind,
            remaining: delay,
            retransmit_timeouts: self.retransmit_timeouts,
            probes_sent: self.probes_sent,
            retransmit_delay: self.rtte.retransmission_timeout(),
            ack_delay: self.ack_delay.unwrap_or(Duration::ZERO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observes_all_active_timer_kinds_and_clamps_elapsed_deadlines() {
        let mut socket = Socket::new(
            SocketBuffer::new(vec![0; 64]),
            SocketBuffer::new(vec![0; 64]),
        );
        let now = Instant::from_millis(100);
        for (timer, kind) in [
            (
                Timer::Retransmit {
                    expires_at: Instant::from_millis(125),
                },
                TimerKind::Retransmit,
            ),
            (
                Timer::Idle {
                    keep_alive_at: Some(Instant::from_millis(125)),
                },
                TimerKind::KeepAlive,
            ),
            (
                Timer::ZeroWindowProbe {
                    expires_at: Instant::from_millis(125),
                    delay: Duration::from_millis(25),
                },
                TimerKind::ZeroWindowProbe,
            ),
            (
                Timer::Close {
                    expires_at: Instant::from_millis(125),
                },
                TimerKind::TimeWait,
            ),
        ] {
            socket.timer = timer;
            let snapshot = socket.timer_observation(now);
            assert_eq!(snapshot.kind, kind);
            assert_eq!(snapshot.remaining, Duration::from_millis(25));
            assert_eq!(
                socket
                    .timer_observation(Instant::from_millis(130))
                    .remaining,
                Duration::ZERO
            );
        }
        socket.timer = Timer::new();
        socket.ack_delay_timer = AckDelayTimer::Waiting(Instant::from_millis(110));
        assert_eq!(socket.timer_observation(now).kind, TimerKind::DelayedAck);
        assert_eq!(
            socket.timer_observation(now).remaining,
            Duration::from_millis(10)
        );
        socket.ack_delay_timer = AckDelayTimer::Idle;
        assert_eq!(socket.timer_observation(now).kind, TimerKind::Inactive);
    }
}
