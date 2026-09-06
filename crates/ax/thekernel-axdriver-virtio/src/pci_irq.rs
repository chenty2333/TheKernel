//! Shared PCI INTx acknowledgment before the interrupt controller's EOI.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use spin::Mutex;
use virtio_drivers::transport::pci::{PciInterruptSource, PciTransport};

const ENDPOINT_COUNT: usize = 64;

struct Endpoint {
    active: AtomicBool,
    readers: AtomicUsize,
    vector: AtomicUsize,
    source: AtomicUsize,
    pending: AtomicU8,
}

impl Endpoint {
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
            vector: AtomicUsize::new(0),
            source: AtomicUsize::new(0),
            pending: AtomicU8::new(0),
        }
    }
}

static ENDPOINTS: [Endpoint; ENDPOINT_COUNT] = [const { Endpoint::new() }; ENDPOINT_COUNT];
// Only admission/teardown take this lock. The interrupt path never does.
static REGISTRY: Mutex<()> = Mutex::new(());

fn dispatch(vector: usize) -> bool {
    let mut owned = false;
    for endpoint in &ENDPOINTS {
        if !endpoint.active.load(Ordering::Acquire)
            || endpoint.vector.load(Ordering::Acquire) != vector
        {
            continue;
        }
        endpoint.readers.fetch_add(1, Ordering::SeqCst);
        if endpoint.active.load(Ordering::SeqCst)
            && endpoint.vector.load(Ordering::Acquire) == vector
        {
            owned = true;
            let address = endpoint.source.load(Ordering::Acquire);
            // Admission publishes a transport ISR capability; teardown masks
            // INTx, deactivates the endpoint and waits for these readers
            // before allowing transport reset/drop or slot reuse.
            let status = unsafe { PciInterruptSource::from_address(address).capture() };
            endpoint.pending.fetch_or(status, Ordering::AcqRel);
        }
        endpoint.readers.fetch_sub(1, Ordering::AcqRel);
    }
    // Direct block callbacks run next and consume the latch before publishing
    // their completion generation. The ordinary IRQ post-hook wakes net and
    // input tasks after all siblings have deasserted the shared line.
    owned
}

fn release(index: usize) {
    let _registry = REGISTRY.lock();
    let endpoint = &ENDPOINTS[index];
    // These operations and the reader's increment/recheck share a total
    // order: either teardown sees the pin, or the reader sees deactivation.
    endpoint.active.store(false, Ordering::SeqCst);
    while endpoint.readers.load(Ordering::SeqCst) != 0 {
        core::hint::spin_loop();
    }
    endpoint.source.store(0, Ordering::Release);
    endpoint.pending.store(0, Ordering::Release);
}

/// Admit the acknowledgment owner before unmasking a PCI function. Drivers
/// without interrupt consumers never call this and retain polling-only INTx.
pub(super) fn admit(transport: &mut PciTransport, vector: usize) -> bool {
    if !axhal::irq::register_shared_dispatcher(dispatch) || !axhal::irq::configure_pci_intx(vector)
    {
        return false;
    }
    let _registry = REGISTRY.lock();
    let Some((index, endpoint)) = ENDPOINTS
        .iter()
        .enumerate()
        .find(|(_, endpoint)| !endpoint.active.load(Ordering::Acquire))
    else {
        return false;
    };
    if !transport.register_interrupt_handler(&endpoint.pending, index, release) {
        return false;
    }
    endpoint.pending.store(0, Ordering::Relaxed);
    endpoint
        .source
        .store(transport.interrupt_source().address(), Ordering::Relaxed);
    endpoint.vector.store(vector, Ordering::Relaxed);
    endpoint.active.store(true, Ordering::Release);
    // Registration above is the prerequisite, so this cannot fail. Failure
    // still leaves the transport responsible for releasing its endpoint.
    transport.enable_interrupts()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_dispatch_latches_only_matching_live_sources() {
        let mut first_isr = 1u8;
        let mut second_isr = 2u8;
        let mut other_isr = 3u8;
        {
            let _registry = REGISTRY.lock();
            for (index, source, vector) in [
                (0, &mut first_isr as *mut u8 as usize, 43),
                (1, &mut second_isr as *mut u8 as usize, 43),
                (2, &mut other_isr as *mut u8 as usize, 42),
            ] {
                let endpoint = &ENDPOINTS[index];
                endpoint.source.store(source, Ordering::Relaxed);
                endpoint.vector.store(vector, Ordering::Relaxed);
                endpoint.active.store(true, Ordering::Release);
            }
        }
        // Local bytes stand in for mapped, byte-wide ISR registers and stay
        // alive until release has synchronized all dispatcher readers.
        assert!(dispatch(43));
        assert_eq!(ENDPOINTS[0].pending.swap(0, Ordering::AcqRel), 1);
        assert_eq!(ENDPOINTS[1].pending.load(Ordering::Acquire), 2);
        assert_eq!(ENDPOINTS[2].pending.load(Ordering::Acquire), 0);
        assert!(dispatch(42));
        assert_eq!(ENDPOINTS[2].pending.load(Ordering::Acquire), 3);
        release(0);
        assert!(dispatch(43));
        release(1);
        assert!(!dispatch(43));
        assert_eq!(ENDPOINTS[1].pending.load(Ordering::Acquire), 0);
        release(2);
    }
}
