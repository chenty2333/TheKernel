use alloc::vec;
use core::cell::RefCell;

use crate::{
    codec::{Widget, find_route},
    desc::{BufferDescriptor, verb},
};
fn widget(node: u8, kind: u32, connections: alloc::vec::Vec<u8>) -> Widget {
    Widget {
        node,
        caps: (kind << 20) | 1,
        pin_caps: 16,
        config: 0,
        connections,
    }
}
#[test]
fn graph_selects_converter_without_codec_id() {
    let nodes = vec![
        widget(2, 0, vec![]),
        widget(3, 3, vec![2]),
        widget(4, 4, vec![3]),
    ];
    let path = find_route(&nodes).unwrap();
    assert_eq!(
        path.iter().map(|w| w.node).collect::<alloc::vec::Vec<_>>(),
        vec![4, 3, 2]
    );
}
#[test]
fn cycle_and_digital_path_are_rejected() {
    let mut nodes = vec![widget(2, 3, vec![4]), widget(4, 4, vec![2])];
    assert!(find_route(&nodes).is_none());
    nodes[0] = widget(2, 0, vec![]);
    nodes[0].caps |= 1 << 9;
    assert!(find_route(&nodes).is_none());
}
#[test]
fn prefers_headphone_and_skips_disconnected_pin() {
    let mut nodes = vec![
        widget(2, 0, vec![]),
        widget(3, 4, vec![2]),
        widget(4, 4, vec![2]),
    ];
    nodes[2].config = 2 << 20;
    assert_eq!(find_route(&nodes).unwrap()[0].node, 4);
    nodes[2].config |= 1 << 30;
    assert_eq!(find_route(&nodes).unwrap()[0].node, 3);
}

struct HdmiVerbs(RefCell<alloc::vec::Vec<(u8, u16, u16)>>);
impl crate::codec::Verbs for HdmiVerbs {
    fn verb(
        &mut self,
        _codec: u8,
        node: u8,
        operation: u16,
        payload: u16,
    ) -> axdriver_base::DevResult<u32> {
        self.0.borrow_mut().push((node, operation, payload));
        Ok(0)
    }
}

fn display_pin(node: u8, converter: u8) -> Widget {
    Widget {
        node,
        caps: (4 << 20) | (1 << 9) | 1,
        pin_caps: (1 << 7) | (1 << 4),
        config: 0,
        connections: vec![converter],
    }
}

#[test]
fn port_specific_hdmi_route_requires_live_digital_pin_and_converter() {
    let mut pin = display_pin(0x0a, 2);
    let mut converter = widget(2, 0, vec![]);
    converter.caps |= 1 << 9;
    let nodes = vec![pin.clone(), converter.clone()];
    assert_eq!(
        crate::codec::find_hdmi_route(&nodes, 0x0a)
            .unwrap()
            .iter()
            .map(|w| w.node)
            .collect::<alloc::vec::Vec<_>>(),
        vec![0x0a, 2]
    );
    assert!(crate::codec::find_hdmi_route(&nodes, 0x0b).is_none());
    pin.pin_caps &= !(1 << 7);
    assert!(crate::codec::find_hdmi_route(&[pin, converter], 0x0a).is_none());
}

#[test]
fn hdmi_setup_emits_two_channel_pcm_and_audio_infoframe_before_enable() {
    let pin = display_pin(0x0a, 2);
    let mut converter = widget(2, 0, vec![]);
    converter.caps |= 1 << 9;
    let route = crate::codec::Route {
        codec: 2,
        function: 1,
        vendor: 0x8086_2815,
        path: vec![pin, converter],
    };
    let mut verbs = HdmiVerbs(RefCell::new(alloc::vec::Vec::new()));
    crate::codec::configure_hdmi(&mut verbs, &route).unwrap();
    let writes = verbs.0.borrow();
    assert!(writes.contains(&(0x0a, 0x707, 0x40)));
    assert!(writes.contains(&(2, 0x200, crate::desc::FORMAT)));
    assert!(writes.contains(&(2, 0x72d, 1)));
    assert!(writes.contains(&(2, 0x706, 0x10)));
    assert!(writes.contains(&(2, 0x70d, 1)));
    assert!(writes.contains(&(0x0a, 0x732, 0xc0)));
    let data = writes
        .iter()
        .filter(|(node, operation, _)| *node == 0x0a && *operation == 0x731)
        .map(|(_, _, payload)| *payload as u8)
        .collect::<alloc::vec::Vec<_>>();
    assert_eq!(data.len(), 14);
    assert_eq!(&data[..5], &[0x84, 1, 10, 0x70, 1]);
    assert_eq!(
        data.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
        0
    );
}
#[test]
fn wire_layout() {
    assert_eq!(core::mem::size_of::<BufferDescriptor>(), 16);
    assert_eq!(verb(2, 5, 0x200, 0x11), 0x20520011);
}

extern crate std;
use core::ptr::NonNull;
use std::{
    alloc::{Layout, alloc_zeroed, dealloc},
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::{
    Controller, Hal, Playback, PlaybackConfig, PlaybackError, PlaybackToken, SampleFormat,
    desc::PERIOD,
    regs::{self, Bus},
};
struct Host;
// SAFETY: aligned host allocation serves as fake physical address.
unsafe impl Hal for Host {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)> {
        // SAFETY: valid nonzero layout; freed with same layout after fake DMA stops.
        let p = NonNull::new(unsafe {
            alloc_zeroed(Layout::from_size_align(pages * 4096, 4096).unwrap())
        })?;
        Some((p.as_ptr() as u64, p))
    }
    unsafe fn release(_: u64, p: NonNull<u8>, pages: usize) {
        // SAFETY: caller has retired fake DMA and retains allocation ownership.
        unsafe {
            dealloc(
                p.as_ptr(),
                Layout::from_size_align(pages * 4096, 4096).unwrap(),
            );
        }
    }
}

static TRACKED_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static TRACKED_RELEASES: AtomicUsize = AtomicUsize::new(0);

struct TrackedHost;
// SAFETY: this wrapper preserves Host's unique, aligned allocation contract.
unsafe impl Hal for TrackedHost {
    fn allocate(pages: usize) -> Option<(u64, NonNull<u8>)> {
        let allocation = Host::allocate(pages)?;
        TRACKED_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        Some(allocation)
    }

    unsafe fn release(address: u64, pointer: NonNull<u8>, pages: usize) {
        // SAFETY: the controller calls this only after a proven shutdown.
        unsafe { Host::release(address, pointer, pages) };
        TRACKED_RELEASES.fetch_add(1, Ordering::Relaxed);
    }
}
#[derive(Default)]
struct State {
    regs: BTreeMap<usize, u32>,
    writes: usize,
    now: u64,
    present: u16,
    verbs: alloc::vec::Vec<(u8, u8, u16, u16)>,
    drop_response_for: Option<(u8, u8, u16, u16)>,
    ignored_writes: alloc::vec::Vec<usize>,
    ignored_next_write: Option<usize>,
}
struct Fake(Arc<Mutex<State>>);
impl Fake {
    fn address(s: &State, o: usize) -> u64 {
        u64::from(s.regs[&o]) | (u64::from(s.regs[&(o + 4)]) << 32)
    }
}
impl Bus for Fake {
    fn read(&mut self, o: usize, _: usize) -> u32 {
        let s = self.0.lock().unwrap();
        match o {
            0 => 0x1101,
            0xe => {
                if s.present == 0 {
                    1
                } else {
                    s.present as u32
                }
            }
            0x4e | 0x5e => 0x40,
            _ => *s.regs.get(&o).unwrap_or(&0),
        }
    }
    fn write(&mut self, o: usize, _: usize, value: u32) {
        let mut s = self.0.lock().unwrap();
        s.writes += 1;
        if s.ignored_writes.contains(&o) {
            return;
        }
        if s.ignored_next_write == Some(o) {
            s.ignored_next_write = None;
            return;
        }
        s.regs.insert(o, value);
        if o == 0xa3 {
            s.regs.insert(o, 0);
        }
        if o == regs::CORBRP || o == regs::RIRBWP {
            s.regs.insert(o, 0);
        }
        if o == regs::CORBWP && value != 0 {
            let corb = Self::address(&s, regs::CORB);
            let rirb = Self::address(&s, regs::RIRB);
            // SAFETY: controller published a valid owned CORB allocation.
            let command = unsafe { (corb as *const u32).add(value as usize).read() };
            let codec = ((command >> 28) & 15) as u8;
            let node = (command >> 20) & 255;
            let operation = (command >> 8) & 4095;
            let payload = command & 255;
            s.verbs
                .push((codec, node as u8, operation as u16, payload as u16));
            if s.drop_response_for == Some((codec, node as u8, operation as u16, payload as u16)) {
                s.drop_response_for = None;
                return;
            }
            let response: u32 = match (codec, node, operation, payload) {
                (0, 0, 0xf00, 0) => 0x10ec0999,
                (0, 0, 0xf00, 4) => 0x10001,
                (0, 1, 0xf00, 5) => 1,
                (0, 1, 0xf00, 4) => 0x20003,
                (0, 2, 0xf00, 9) => 17,
                (0, 3, 0xf00, 9) => 0x300100,
                (0, 4, 0xf00, 9) => 0x400100,
                (0, 2, 0xf00, 0xa) => (1 << 6) | (1 << 17),
                (0, 2, 0xf00, 0xb) => 1,
                (0, 4, 0xf00, 0xc) => 16,
                (0, 3 | 4, 0xf00, 0xe) => 1,
                (0, 3, 0xf02, _) => 2,
                (0, 4, 0xf02, _) => 3,
                (2, 0, 0xf00, 0) => 0x80862815,
                (2, 0, 0xf00, 4) => 0x10001,
                (2, 1, 0xf00, 5) => 1,
                (2, 1, 0xf00, 4) => 0x20009,
                (2, 2, 0xf00, 9) => 1 << 9,
                (2, 10, 0xf00, 9) => (4 << 20) | (1 << 9) | (1 << 8),
                (2, 10, 0xf00, 0xc) => (1 << 7) | (1 << 4),
                (2, 10, 0xf00, 0x0e) => 1,
                (2, 10, 0xf02, _) => 2,
                (2, 1, 0xf00, 0xa) => (1 << 6) | (1 << 17),
                (2, 1, 0xf00, 0xb) => 1,
                (2, 2, 0xf00, 0xa) => (1 << 6) | (1 << 17),
                (2, 2, 0xf00, 0xb) => 1,
                _ => 0,
            };
            let wp = (*s.regs.get(&regs::RIRBWP).unwrap_or(&0) + 1) & 255;
            s.regs.insert(regs::RIRBWP, wp);
            // SAFETY: owned RIRB allocation is large enough for this published entry.
            unsafe {
                (rirb as *mut u64)
                    .add(wp as usize)
                    .write(u64::from(response) | (u64::from(codec) << 32));
            }
        }
        if o == 0xa0 && value & 1 != 0 {
            s.regs.insert(0xa4, 0);
        }
    }
    fn delay_us(&mut self, m: u32) {
        self.0.lock().unwrap().now += u64::from(m) * 1000;
    }
    fn now_ns(&self) -> u64 {
        self.0.lock().unwrap().now
    }
}
#[test]
fn controller_route_bdl_and_period_completion() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut c = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    assert_eq!(c.route().vendor, 0x10ec0999);
    assert_eq!(c.route().path.last().unwrap().node, 2);
    assert_eq!(state.lock().unwrap().regs[&regs::RIRBCTL], 3);
    assert_eq!(state.lock().unwrap().regs[&0x20], 0);
    c.prepare(4096, 4).unwrap();
    let token = c.submit(&[0x55; PERIOD]).unwrap();
    let second = c.submit(&[0x66; PERIOD]).unwrap();
    assert_ne!(token, second);
    {
        let mut s = state.lock().unwrap();
        let bdl = Fake::address(&s, 0xb8);
        // SAFETY: stopped fake hardware only inspects the driver's owned BDL and audio memory.
        unsafe {
            let first = (bdl as *const BufferDescriptor).read();
            assert_eq!(first.length, 4096);
            assert_eq!(*(first.address as *const u8), 0x55);
        }
        s.regs.insert(0xa4, 4096);
        s.now += 22_000_000;
    }
    assert_eq!(c.complete().unwrap(), Some(token));
    {
        let mut s = state.lock().unwrap();
        s.regs.insert(0xa4, 8192);
        s.now += 22_000_000;
    }
    assert_eq!(c.complete().unwrap(), Some(second));
    let before = state.lock().unwrap().now;
    c.release().unwrap();
    assert!(state.lock().unwrap().now - before >= 42_666_000);
}

#[test]
fn command_rings_require_enable_readback_before_codec_verbs() {
    for ring in [regs::RIRBCTL, regs::CORBCTL] {
        let state = Arc::new(Mutex::new(State {
            ignored_writes: alloc::vec![ring],
            ..State::default()
        }));
        assert!(Controller::<Host, _>::new(Fake(state.clone())).is_err());
        assert!(
            state.lock().unwrap().verbs.is_empty(),
            "codec verbs must not run when {ring:#x} failed to enable"
        );
    }
}

#[test]
fn stream_run_requires_readback_before_submit_succeeds() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut controller = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    controller.prepare(PERIOD as u32, 4).unwrap();
    state.lock().unwrap().ignored_next_write = Some(0xa0);

    assert!(matches!(
        controller.submit(&[0x55; PERIOD]),
        Err(axdriver_base::DevError::Io)
    ));
    assert_eq!(state.lock().unwrap().regs[&0xa0] & 2, 0);
    let writes_after_failure = state.lock().unwrap().writes;
    assert!(matches!(
        controller.complete(),
        Err(axdriver_base::DevError::Io)
    ));
    assert_eq!(state.lock().unwrap().writes, writes_after_failure);
    drop(controller);
}

#[test]
fn configure_rejects_input_amp_index_overflow_before_codec_mutations() {
    let route = crate::codec::Route {
        codec: 0,
        function: 1,
        vendor: 0,
        path: alloc::vec![
            Widget {
                node: 1,
                caps: (4 << 20) | (1 << 8),
                pin_caps: 1 << 4,
                config: 0,
                connections: alloc::vec![2],
            },
            Widget {
                node: 2,
                caps: (3 << 20) | (1 << 1) | (1 << 8),
                pin_caps: 0,
                config: 0,
                connections: (3..20).collect(),
            },
            Widget {
                node: 20,
                caps: 1,
                pin_caps: 0,
                config: 0,
                connections: alloc::vec![],
            },
        ],
    };
    let mut verbs = HdmiVerbs(RefCell::new(alloc::vec::Vec::new()));

    assert!(matches!(
        crate::codec::configure(&mut verbs, &route),
        Err(axdriver_base::DevError::Unsupported)
    ));
    assert!(verbs.0.borrow().is_empty());
}

#[test]
fn submit_observes_completions_and_requires_token_collection_before_reuse() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut controller = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    controller.prepare(PERIOD as u32, 4).unwrap();
    let first = controller.submit(&[0x55; PERIOD]).unwrap();

    {
        let mut observed = state.lock().unwrap();
        observed.regs.insert(0xa4, PERIOD as u32);
        observed.now += 20_000_000;
    }
    assert!(matches!(
        controller.submit(&[0x66; PERIOD]),
        Err(axdriver_base::DevError::Again)
    ));
    assert!(matches!(
        controller.prepare(PERIOD as u32, 4),
        Err(axdriver_base::DevError::ResourceBusy)
    ));
    assert_eq!(controller.complete().unwrap(), Some(first));
    let second = controller.submit(&[0x66; PERIOD]).unwrap();
    assert_ne!(first, second);
    controller.abort().unwrap();
}

#[test]
fn failed_stream_stop_preserves_dma_ownership_until_retry() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut controller = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    controller.prepare(PERIOD as u32, 4).unwrap();
    controller.submit(&[0x55; PERIOD]).unwrap();
    state.lock().unwrap().ignored_next_write = Some(0xa0);

    assert!(matches!(
        controller.abort(),
        Err(axdriver_base::DevError::Io)
    ));
    assert_eq!(
        state.lock().unwrap().regs[&0xa0] & 2,
        2,
        "the ignored stop leaves RUN asserted"
    );
    let writes_after_failure = state.lock().unwrap().writes;
    assert!(controller.prepare(PERIOD as u32, 4).is_err());
    assert!(controller.submit(&[0x66; PERIOD]).is_err());
    assert_eq!(state.lock().unwrap().writes, writes_after_failure);

    // The one-shot fault is gone. Drop can now prove the stream stopped and
    // reset the controller before releasing its DMA allocations.
    drop(controller);
}

#[test]
fn display_eld_selects_confirmed_codec_pin_and_retires_playback_before_restore() {
    let state = Arc::new(Mutex::new(State {
        present: 1 | (1 << 2),
        ..State::default()
    }));
    let mut controller = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    let mut eld = [0u8; 24];
    eld[0] = 0x10;
    eld[2] = 5;
    eld[4] = 3 << 5;
    eld[5] = 1 << 4;
    eld[20..23].copy_from_slice(&[0x09, 1 << 2, 1]);

    if let Err(error) = controller.set_display_eld(3, Some(&eld)) {
        let verbs = state.lock().unwrap().verbs.clone();
        drop(controller);
        panic!("set ELD: {error:?}; verbs={verbs:?}");
    }
    {
        let observed = state.lock().unwrap();
        assert!(observed.verbs.contains(&(2, 0x0a, 0x707, 0x40)));
        assert!(observed.verbs.contains(&(2, 2, 0x706, 0x10)));
        assert!(observed.verbs.contains(&(2, 2, 0x70d, 1)));
    }
    controller.prepare(4096, 4).unwrap();
    let _token = controller.submit(&[0x5a; PERIOD]).unwrap();
    controller.set_display_eld(3, None).unwrap();
    controller.prepare(4096, 4).unwrap();
    assert!(matches!(
        controller.submit(&[0x66; PERIOD]),
        Err(axdriver_base::DevError::Io)
    ));
    assert!(matches!(
        controller.complete(),
        Err(axdriver_base::DevError::Io)
    ));
    assert_eq!(controller.complete().unwrap(), None);
    controller.prepare(4096, 4).unwrap();
    controller.submit(&[0x77; PERIOD]).unwrap();
    controller.abort().unwrap();

    // An owner may explicitly abort after the one-shot route interruption to
    // discard its old generation and resume on the replacement route.
    controller.set_display_eld(3, Some(&eld)).unwrap();
    controller.prepare(4096, 4).unwrap();
    controller.submit(&[0x55; PERIOD]).unwrap();
    controller.set_display_eld(3, None).unwrap();
    controller.abort().unwrap();
    controller.prepare(4096, 4).unwrap();
    controller.submit(&[0x66; PERIOD]).unwrap();
    controller.abort().unwrap();
    assert!(state.lock().unwrap().verbs.contains(&(2, 2, 0x706, 0)));
}

#[test]
fn partial_hdmi_verb_timeout_invalidates_uncertain_controller_state() {
    let state = Arc::new(Mutex::new(State {
        present: 1 | (1 << 2),
        ..State::default()
    }));
    let mut controller = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    let mut eld = [0u8; 24];
    eld[0] = 0x10;
    eld[2] = 5;
    eld[4] = 3 << 5;
    eld[5] = 1 << 4;
    eld[20..23].copy_from_slice(&[0x09, 1 << 2, 1]);
    state.lock().unwrap().drop_response_for = Some((2, 2, 0x200, crate::desc::FORMAT));

    assert!(matches!(
        controller.set_display_eld(3, Some(&eld)),
        Err(axdriver_base::DevError::Io)
    ));
    assert_eq!(controller.route().vendor, 0x10ec0999);
    let (drop_response_for, verbs) = {
        let observed = state.lock().unwrap();
        (observed.drop_response_for, observed.verbs.clone())
    };
    assert_eq!(drop_response_for, None);
    let failed = verbs
        .iter()
        .position(|verb| *verb == (2, 2, 0x200, crate::desc::FORMAT))
        .unwrap();
    assert!(verbs[failed + 1..].is_empty());
    assert!(
        matches!(
            controller.prepare(4096, 4),
            Err(axdriver_base::DevError::InvalidParam)
        ),
        "an unanswered partially landed codec verb invalidates playback"
    );
}
#[test]
fn missed_full_lap_is_not_fabricated_completion() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut c = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    c.prepare(4096, 4).unwrap();
    c.submit(&[0; PERIOD]).unwrap();
    state.lock().unwrap().now += 100_000_000;
    assert!(c.complete().is_err());
}
#[test]
fn abort_stops_before_discarding_and_allows_a_fresh_stream() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut c = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    c.prepare(4096, 4).unwrap();
    c.submit(&[0x55; PERIOD]).unwrap();
    c.abort().unwrap();
    assert_eq!(state.lock().unwrap().regs[&0xa0] & 2, 0);
    assert_eq!(c.complete().unwrap(), None);
    c.prepare(4096, 4).unwrap();
    c.submit(&[0x77; PERIOD]).unwrap();
    c.abort().unwrap();
}

#[test]
fn rdif_playback_copies_periods_retains_order_and_supports_reprepare() {
    let state = Arc::new(Mutex::new(State::default()));
    let mut controller = Controller::<Host, _>::new(Fake(state.clone())).unwrap();
    let config = PlaybackConfig {
        sample_format: SampleFormat::S16Le,
        sample_rate_hz: 48_000,
        channels: 2,
        period_frames: 1024,
        period_count: 4,
    };
    assert_eq!(controller.configurations(), &[config]);
    assert_eq!(config.period_bytes(), Some(PERIOD));
    assert_eq!(controller.max_poll_interval_ns(), 10_000_000);

    let wrong_config = PlaybackConfig {
        sample_rate_hz: 44_100,
        ..config
    };
    let writes = state.lock().unwrap().writes;
    assert_eq!(
        Playback::prepare(&mut controller, wrong_config),
        Err(PlaybackError::Unsupported)
    );
    assert_eq!(state.lock().unwrap().writes, writes);
    Playback::prepare(&mut controller, config).unwrap();

    let mut first_pcm = alloc::vec![0x55; PERIOD];
    let first = Playback::submit(&mut controller, &first_pcm).unwrap();
    first_pcm.fill(0x66);
    let second = Playback::submit(&mut controller, &[0x77; PERIOD]).unwrap();
    {
        let observed = state.lock().unwrap();
        let bdl = Fake::address(&observed, 0xb8);
        // SAFETY: the fake controller only reads the driver's live DMA pages.
        unsafe {
            let first_desc = (bdl as *const BufferDescriptor).read();
            let second_desc = (bdl as *const BufferDescriptor).add(1).read();
            assert_eq!(*(first_desc.address as *const u8), 0x55);
            assert_eq!(*(second_desc.address as *const u8), 0x77);
        }
    }

    {
        let mut observed = state.lock().unwrap();
        observed.regs.insert(0xa4, (PERIOD * 2) as u32);
        observed.now += 22_000_000;
    }
    assert_eq!(
        Playback::complete(&mut controller).unwrap(),
        Some(PlaybackToken(first.0))
    );
    Playback::release(&mut controller).unwrap();
    assert_eq!(
        Playback::complete(&mut controller).unwrap(),
        Some(PlaybackToken(second.0)),
        "release must preserve a completed but uncollected token"
    );

    Playback::prepare(&mut controller, config).unwrap();
    let cancelled = Playback::submit(&mut controller, &[0x33; PERIOD]).unwrap();
    Playback::abort(&mut controller).unwrap();
    assert_eq!(Playback::complete(&mut controller).unwrap(), None);
    Playback::prepare(&mut controller, config).unwrap();
    let _fresh = Playback::submit(&mut controller, &[0x44; PERIOD]).unwrap();
    assert_ne!(cancelled, _fresh);
    Playback::shutdown(&mut controller).unwrap();
    assert_eq!(state.lock().unwrap().regs[&regs::GCTL], 0);
    assert_eq!(
        Playback::prepare(&mut controller, config),
        Err(PlaybackError::BadState)
    );
}

#[test]
fn shutdown_releases_only_buffers_with_engine_stop_proof() {
    let allocations_before = TRACKED_ALLOCATIONS.load(Ordering::Relaxed);
    let releases_before = TRACKED_RELEASES.load(Ordering::Relaxed);
    let state = Arc::new(Mutex::new(State::default()));
    let mut controller = Controller::<TrackedHost, _>::new(Fake(state.clone())).unwrap();
    assert_eq!(
        TRACKED_ALLOCATIONS.load(Ordering::Relaxed) - allocations_before,
        4
    );
    Playback::shutdown(&mut controller).unwrap();
    assert_eq!(TRACKED_RELEASES.load(Ordering::Relaxed), releases_before);
    drop(controller);
    assert_eq!(
        TRACKED_RELEASES.load(Ordering::Relaxed) - releases_before,
        4,
        "the four owned DMA allocations release only after shutdown"
    );

    let allocations_before = TRACKED_ALLOCATIONS.load(Ordering::Relaxed);
    let releases_before = TRACKED_RELEASES.load(Ordering::Relaxed);
    let state = Arc::new(Mutex::new(State::default()));
    let mut controller = Controller::<TrackedHost, _>::new(Fake(state.clone())).unwrap();
    assert_eq!(
        TRACKED_ALLOCATIONS.load(Ordering::Relaxed) - allocations_before,
        4
    );
    controller.prepare(PERIOD as u32, 4).unwrap();
    controller.submit(&[0x55; PERIOD]).unwrap();
    state
        .lock()
        .unwrap()
        .ignored_writes
        .extend([0xa0, regs::GCTL]);
    assert_eq!(
        Playback::shutdown(&mut controller),
        Err(PlaybackError::Device)
    );
    drop(controller);
    assert_eq!(
        TRACKED_RELEASES.load(Ordering::Relaxed),
        releases_before + 2,
        "stopped CORB/RIRB buffers release, while active stream DMA stays quarantined"
    );
}
