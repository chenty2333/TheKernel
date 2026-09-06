use alloc::{string::String, sync::Arc};
use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
};

use axdriver::prelude::*;
use axerrno::{AxError, AxResult, ax_bail};
use axpoll::{PollSet, RegisterError, UpdateError};
use axsync::Mutex;
use axtask::{
    WaitQueue,
    future::{
        IrqWakerRegisterError, IrqWakerToken, IrqWakerUpdateError, block_on, cancel_irq_waker,
        interruptible, register_irq_waker, update_irq_waker,
    },
};

use crate::vsock::connection_manager::{VSOCK_CONN_MANAGER, VsockConnectionManager};

// we need a global and static only one vsock device
static VSOCK_DEVICE: Mutex<Option<AxVsockDevice>> = Mutex::new(None);
static VSOCK_IRQ: Mutex<Option<usize>> = Mutex::new(None);

const PENDING_EVENT_CAPACITY: usize = 256;

struct PendingEvents {
    slots: [Option<VsockDriverEvent>; PENDING_EVENT_CAPACITY],
    head: usize,
    len: usize,
}

impl PendingEvents {
    const fn new() -> Self {
        Self {
            slots: [const { None }; PENDING_EVENT_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push_back(&mut self, event: VsockDriverEvent) -> Result<(), VsockDriverEvent> {
        if self.len == PENDING_EVENT_CAPACITY {
            return Err(event);
        }
        let index = (self.head + self.len) % PENDING_EVENT_CAPACITY;
        self.slots[index] = Some(event);
        self.len += 1;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<VsockDriverEvent> {
        if self.len == 0 {
            return None;
        }
        let event = self.slots[self.head].take();
        self.head = (self.head + 1) % PENDING_EVENT_CAPACITY;
        self.len -= 1;
        event
    }

    const fn len(&self) -> usize {
        self.len
    }
}

static PENDING_EVENTS: Mutex<PendingEvents> = Mutex::new(PendingEvents::new());

const VSOCK_RX_TMPBUF_SIZE: usize = 0x1000; // 4KiB buffer for vsock receive

/// Stable wake handles produced by one event transaction. State mutation is
/// complete before these handles are published, and publication happens only
/// after both the device and connection-manager locks have been released.
#[derive(Default)]
struct DeferredNotifications {
    accept_poll: Option<Arc<PollSet>>,
    rx_poll: Option<Arc<PollSet>>,
    connect_poll: Option<Arc<PollSet>>,
    tx_wait: Option<Arc<WaitQueue>>,
}

impl DeferredNotifications {
    fn publish(self) {
        if let Some(source) = self.accept_poll {
            source.wake();
        }
        if let Some(source) = self.rx_poll {
            source.wake();
        }
        if let Some(source) = self.connect_poll {
            source.wake();
        }
        if let Some(wait_queue) = self.tx_wait {
            // WaitQueue notification may yield when rescheduling is requested;
            // it therefore belongs strictly outside every vsock state lock.
            wait_queue.notify_all(true);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventProgress {
    Progress,
    Deferred,
}

struct EventOutcome {
    progress: EventProgress,
    notifications: DeferredNotifications,
}

impl EventOutcome {
    fn progress() -> Self {
        Self {
            progress: EventProgress::Progress,
            notifications: DeferredNotifications::default(),
        }
    }

    fn deferred() -> Self {
        Self {
            progress: EventProgress::Deferred,
            notifications: DeferredNotifications::default(),
        }
    }

    fn received(read_len: usize, remaining: usize) -> Self {
        if read_len == 0 && remaining != 0 {
            Self::deferred()
        } else {
            Self::progress()
        }
    }
}

fn publish_event_outcome(made_progress: &mut bool, outcome: EventOutcome) {
    *made_progress |= outcome.progress == EventProgress::Progress;
    outcome.notifications.publish();
}

/// Registers a vsock device. Only one vsock device can be registered.
pub fn register_vsock_device(dev: AxVsockDevice) -> AxResult {
    let mut guard = VSOCK_DEVICE.lock();
    if guard.is_some() {
        ax_bail!(AlreadyExists, "vsock device already registered");
    }
    let irq = dev.irq_num().ok_or(AxError::OperationNotSupported)?;
    // This wrapper exclusively owns the discovered vsock device and its IRQ
    // capability. Generic waker registration never enables hardware.
    axhal::irq::set_enable(irq, true);
    *guard = Some(dev);
    *VSOCK_IRQ.lock() = Some(irq);
    drop(guard);
    Ok(())
}

static POLL_REF_COUNT: Mutex<usize> = Mutex::new(0);
static POLL_TASK_RUNNING: AtomicBool = AtomicBool::new(false);
static EVENT_TASK: Mutex<Option<axtask::AxTaskRef>> = Mutex::new(None);

fn rollback_poll_charge(count: &mut usize) -> AxResult<()> {
    let previous = count.checked_sub(1).ok_or(AxError::BadState)?;
    *count = previous;
    Ok(())
}

fn rollback_vsock_poll_charge() -> AxResult<()> {
    let mut count = POLL_REF_COUNT.lock();
    rollback_poll_charge(&mut count)
}

pub fn start_vsock_poll() -> AxResult<()> {
    let mut count = POLL_REF_COUNT.lock();
    *count = count.checked_add(1).ok_or(AxError::OutOfRange)?;
    let new_count = *count;
    debug!("start_vsock_poll: ref_count -> {new_count}");
    if !POLL_TASK_RUNNING.swap(true, Ordering::SeqCst) {
        drop(count);
        debug!("Starting IRQ-backed vsock event task");
        let mut name = String::new();
        if name.try_reserve_exact("vsock-event".len()).is_err() {
            POLL_TASK_RUNNING.store(false, Ordering::SeqCst);
            rollback_vsock_poll_charge()?;
            return Err(AxError::NoMemory);
        }
        name.push_str("vsock-event");
        match axtask::spawn_with_name(vsock_poll_loop, name) {
            Ok(task) => *EVENT_TASK.lock() = Some(task),
            Err(error) => {
                POLL_TASK_RUNNING.store(false, Ordering::SeqCst);
                rollback_vsock_poll_charge()?;
                return Err(error);
            }
        }
    }
    Ok(())
}

pub fn stop_vsock_poll() {
    let mut count = POLL_REF_COUNT.lock();
    if *count == 0 {
        // this should not happen, log a warning
        warn!("stop_vsock_poll called but ref_count already 0");
        return;
    }
    *count -= 1;
    let new_count = *count;
    debug!("stop_vsock_poll: ref_count -> {new_count}");
}

/// Wakes the event task after userspace frees receive-buffer capacity needed
/// by a bounded deferred driver event.
pub fn notify_vsock_rx_capacity() {
    if let Some(task) = EVENT_TASK.lock().as_ref() {
        task.interrupt();
    }
}

fn vsock_poll_loop() {
    let Some(irq) = *VSOCK_IRQ.lock() else {
        warn!("vsock event task started without an IRQ source");
        POLL_TASK_RUNNING.store(false, Ordering::SeqCst);
        return;
    };
    let mut irq_registration = VsockIrqRegistration::new(irq);
    loop {
        if let Err(error) = irq_registration.arm() {
            warn!("vsock IRQ registration failed: {error:?}");
            break;
        }

        match poll_vsock_interfaces() {
            Ok(true) => {
                irq_registration.cancel();
                axtask::yield_now();
                continue;
            }
            Ok(false) => {}
            Err(error) => {
                irq_registration.cancel();
                warn!("vsock device poll failed: {error:?}");
                break;
            }
        }

        let waited = block_on(interruptible(poll_fn(|context| {
            irq_registration.poll_wait(context)
        })));
        // The wait phase only updates the already-published IRQ registration.
        // Cancellation and driver/manager work remain outside the block session.
        irq_registration.cancel();
        match waited {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                warn!("vsock IRQ wait failed: {error:?}");
                break;
            }
            Ok(Err(_)) => {
                axtask::current().clear_interrupt();
            }
            Err(error) => {
                warn!("vsock poll task could not block: {error:?}");
                break;
            }
        }
    }
    EVENT_TASK.lock().take();
    POLL_TASK_RUNNING.store(false, Ordering::SeqCst);
}

struct VsockIrqRegistration {
    irq: usize,
    token: Option<IrqWakerToken>,
}

impl VsockIrqRegistration {
    const fn new(irq: usize) -> Self {
        Self { irq, token: None }
    }

    fn arm(&mut self) -> AxResult<()> {
        if self.token.is_none() {
            self.token =
                Some(register_irq_waker(self.irq, Waker::noop()).map_err(map_irq_wait_error)?);
        }
        Ok(())
    }

    fn cancel(&mut self) {
        if let Some(token) = self.token.take() {
            cancel_irq_waker(token);
        }
    }

    fn poll_wait(&mut self, context: &mut Context<'_>) -> Poll<AxResult<()>> {
        let Some(token) = self.token else {
            return Poll::Ready(Err(AxError::BadState));
        };
        match update_irq_waker(token, context.waker()) {
            Ok(()) => Poll::Pending,
            Err(IrqWakerUpdateError::Registration(UpdateError::InvalidToken)) => {
                self.token = None;
                Poll::Ready(Ok(()))
            }
            Err(IrqWakerUpdateError::Registration(UpdateError::Closed))
            | Err(IrqWakerUpdateError::InvalidSource) => Poll::Ready(Err(AxError::BadState)),
        }
    }
}

impl Drop for VsockIrqRegistration {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn map_irq_wait_error(error: IrqWakerRegisterError) -> AxError {
    match error {
        IrqWakerRegisterError::Waiter(RegisterError::Full)
        | IrqWakerRegisterError::SourceCapacityExhausted
        | IrqWakerRegisterError::HookInstallationInProgress => AxError::ResourceBusy,
        IrqWakerRegisterError::Waiter(RegisterError::TokenSpaceExhausted) => AxError::OutOfRange,
        IrqWakerRegisterError::Waiter(RegisterError::Closed)
        | IrqWakerRegisterError::HookUnavailable => AxError::BadState,
    }
}

fn poll_vsock_interfaces() -> AxResult<bool> {
    if VSOCK_DEVICE.lock().is_none() {
        return Err(AxError::NotFound);
    }
    let mut made_progress = false;
    let mut buf = [0; VSOCK_RX_TMPBUF_SIZE];

    // Process at most the fixed pending capacity before polling fresh device
    // events. Events requeued for backpressure are deferred to the next
    // bounded worker iteration.
    let pending_budget = PENDING_EVENTS.lock().len();
    for _ in 0..pending_budget {
        let Some(event) = PENDING_EVENTS.lock().pop_front() else {
            break;
        };
        let outcome = {
            let mut guard = VSOCK_DEVICE.lock();
            let dev = guard.as_mut().ok_or(AxError::NotFound)?;
            handle_vsock_event(event, dev, &mut buf)
        };
        publish_event_outcome(&mut made_progress, outcome);
    }

    const DRIVER_EVENT_BUDGET: usize = 256;
    for _ in 0..DRIVER_EVENT_BUDGET {
        let outcome = {
            let mut guard = VSOCK_DEVICE.lock();
            let dev = guard.as_mut().ok_or(AxError::NotFound)?;
            dev.poll_event()
                .map_err(map_dev_err)?
                .map(|event| handle_vsock_event(event, dev, &mut buf))
        };
        let Some(outcome) = outcome else {
            break;
        };
        publish_event_outcome(&mut made_progress, outcome);
    }
    Ok(made_progress)
}

fn abort_overflowed_connection(
    dev: &mut AxVsockDevice,
    manager: &mut VsockConnectionManager,
    conn_id: crate::vsock::VsockConnId,
    outcome: &mut EventOutcome,
) {
    outcome.progress = EventProgress::Progress;
    if let Err(error) = dev.abort(conn_id) {
        warn!("failed to abort overflowed vsock connection: {error:?}");
    }
    match manager.on_disconnected(conn_id) {
        Ok((rx_poll, connect_poll)) => {
            outcome.notifications.rx_poll = rx_poll;
            outcome.notifications.connect_poll = connect_poll;
        }
        Err(error) => warn!("failed to publish overflow disconnect: {error:?}"),
    }
}

fn handle_vsock_event(
    event: VsockDriverEvent,
    dev: &mut AxVsockDevice,
    buf: &mut [u8],
) -> EventOutcome {
    let mut manager = VSOCK_CONN_MANAGER.lock();
    debug!("Handling vsock event: {event:?}");

    match event {
        VsockDriverEvent::ConnectionRequest(conn_id) => {
            let mut outcome = EventOutcome::progress();
            match manager.on_connection_request(conn_id) {
                Ok(accept_poll) => outcome.notifications.accept_poll = accept_poll,
                Err(e) => {
                    info!("Connection request failed: {conn_id:?}, error={e:?}");
                    // The VirtIO layer has already sent its acceptance before
                    // publishing ConnectionRequest. Finish the upper-state
                    // rollback, release its lock, and explicitly reject the
                    // lower connection so the two layers cannot diverge.
                    drop(manager);
                    if let Err(error) = dev.abort(conn_id) {
                        warn!("failed to abort rejected vsock connection: {error:?}");
                    }
                }
            }
            outcome
        }

        VsockDriverEvent::Received(conn_id, len) => {
            let free_space = if let Some(conn) = manager.get_connection(conn_id) {
                conn.lock().rx_buffer_free()
            } else {
                info!("Received data for unknown connection: {conn_id:?}");
                return EventOutcome::progress();
            };

            if free_space == 0 {
                if PENDING_EVENTS
                    .lock()
                    .push_back(VsockDriverEvent::Received(conn_id, len))
                    .is_err()
                {
                    warn!("bounded vsock deferred-event queue is full; aborting {conn_id:?}");
                    let mut outcome = EventOutcome::progress();
                    abort_overflowed_connection(dev, &mut manager, conn_id, &mut outcome);
                    return outcome;
                }
                return EventOutcome::deferred();
            }

            let max_read = free_space.min(buf.len()).min(len);
            match dev.recv(conn_id, &mut buf[..max_read]) {
                Ok(read_len) => {
                    let remaining = len.saturating_sub(read_len);
                    let mut outcome = EventOutcome::received(read_len, remaining);
                    match manager.on_data_received(conn_id, &buf[..read_len]) {
                        Ok(rx_poll) => outcome.notifications.rx_poll = rx_poll,
                        Err(e) => info!(
                            "Failed to handle received data: conn_id={conn_id:?}, error={e:?}",
                        ),
                    }
                    if remaining != 0
                        && PENDING_EVENTS
                            .lock()
                            .push_back(VsockDriverEvent::Received(conn_id, remaining))
                            .is_err()
                    {
                        warn!(
                            "bounded vsock deferred-event queue overflowed a partial receive; \
                             aborting {conn_id:?}"
                        );
                        abort_overflowed_connection(dev, &mut manager, conn_id, &mut outcome);
                    }
                    outcome
                }
                Err(e) => {
                    info!("Failed to receive vsock data: conn_id={conn_id:?}, error={e:?}",);
                    EventOutcome::progress()
                }
            }
        }

        VsockDriverEvent::Disconnected(conn_id) => {
            let mut outcome = EventOutcome::progress();
            match manager.on_disconnected(conn_id) {
                Ok((rx_poll, connect_poll)) => {
                    outcome.notifications.rx_poll = rx_poll;
                    outcome.notifications.connect_poll = connect_poll;
                }
                Err(e) => info!("Failed to handle disconnection: {conn_id:?}, error={e:?}",),
            }
            outcome
        }

        VsockDriverEvent::Connected(conn_id) => {
            let mut outcome = EventOutcome::progress();
            match manager.on_connected(conn_id) {
                Ok(connect_poll) => outcome.notifications.connect_poll = connect_poll,
                Err(e) => {
                    info!("Failed to handle connection established: {conn_id:?}, error={e:?}",)
                }
            }
            outcome
        }

        VsockDriverEvent::CreditUpdate(conn_id) => {
            let mut outcome = EventOutcome::progress();
            match manager.on_credit_update(conn_id) {
                Ok(tx_wait) => outcome.notifications.tx_wait = tx_wait,
                Err(e) => warn!("Failed to handle credit update: {conn_id:?}, error={e:?}"),
            }
            outcome
        }

        VsockDriverEvent::Unknown => {
            warn!("Received unknown vsock event");
            EventOutcome::progress()
        }
    }
}

pub fn vsock_listen(addr: VsockAddr) -> AxResult<()> {
    let mut guard = VSOCK_DEVICE.lock();
    let dev = guard.as_mut().ok_or(AxError::NotFound)?;
    dev.listen(addr.port);
    Ok(())
}

fn map_dev_err(e: DevError) -> AxError {
    match e {
        DevError::AlreadyExists => AxError::AlreadyExists,
        DevError::Again => AxError::WouldBlock,
        DevError::BadState => AxError::BadState,
        DevError::InvalidParam => AxError::InvalidInput,
        DevError::Io => AxError::Io,
        DevError::NoMemory => AxError::NoMemory,
        DevError::ResourceBusy => AxError::ResourceBusy,
        DevError::Unsupported => AxError::Unsupported,
    }
}

pub fn vsock_connect(conn_id: VsockConnId) -> AxResult<()> {
    let mut guard = VSOCK_DEVICE.lock();
    let dev = guard.as_mut().ok_or(AxError::NotFound)?;
    dev.connect(conn_id).map_err(map_dev_err)
}

pub fn vsock_send(conn_id: VsockConnId, buf: &[u8]) -> AxResult<usize> {
    let max_retries = 10; // Tests have shown that no more than two retries will be notified
    for _ in 0..max_retries {
        let result = {
            let mut guard = VSOCK_DEVICE.lock();
            let dev = guard.as_mut().ok_or(AxError::NotFound)?;
            dev.send(conn_id, buf)
        };
        match result {
            Ok(len) => return Ok(len),
            Err(DevError::Again) => {
                let connection = VSOCK_CONN_MANAGER.lock().get_connection(conn_id);
                let tx_wait = connection.map(|conn| conn.lock().tx_wait_source());
                if let Some(tx_wait) = tx_wait {
                    tx_wait
                        .wait_timeout(core::time::Duration::from_millis(10))
                        .map_err(AxError::from)?;
                };
            }
            Err(e) => return Err(map_dev_err(e)),
        }
    }
    Err(map_dev_err(DevError::Again))
}

pub fn vsock_disconnect(conn_id: VsockConnId) -> AxResult<()> {
    let mut guard = VSOCK_DEVICE.lock();
    let dev = guard.as_mut().ok_or(AxError::NotFound)?;
    dev.disconnect(conn_id).map_err(map_dev_err)
}

pub fn vsock_guest_cid() -> AxResult<u64> {
    let mut guard = VSOCK_DEVICE.lock();
    let dev = guard.as_mut().ok_or(AxError::NotFound)?;
    Ok(dev.guest_cid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_charge_rollback_is_exact_and_preserves_underflow_state() {
        let mut count = 2;
        rollback_poll_charge(&mut count).unwrap();
        assert_eq!(count, 1);

        let mut empty = 0;
        assert_eq!(rollback_poll_charge(&mut empty), Err(AxError::BadState));
        assert_eq!(empty, 0);
    }

    #[test]
    fn deferred_receive_needs_external_capacity_before_busy_retry() {
        assert_eq!(
            EventOutcome::received(0, 1).progress,
            EventProgress::Deferred
        );
        assert_eq!(
            EventOutcome::received(1, 1).progress,
            EventProgress::Progress
        );
        assert_eq!(
            EventOutcome::received(0, 0).progress,
            EventProgress::Progress
        );

        let mut made_progress = false;
        publish_event_outcome(&mut made_progress, EventOutcome::deferred());
        assert!(!made_progress);
        publish_event_outcome(&mut made_progress, EventOutcome::progress());
        assert!(made_progress);
    }
}
