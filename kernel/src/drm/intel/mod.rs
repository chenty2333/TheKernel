//! The Intel display driver's foundation: find the device, identify it, read
//! its registers, and say what happened.
//!
//! This is the first Intel-specific code in the kernel.  It exists to answer
//! one question honestly -- *is this kernel talking to a real Intel GPU?* --
//! before anything is built on top of the answer.  It does that without
//! programming the device at all: it reads PCI configuration space, matches
//! what it finds against a table of known parts, maps the register aperture
//! read-only in effect, and reads the handful of registers that are safe to
//! read on a part whose display engine the firmware is still driving.
//!
//! ## Layout
//!
//! | module | what it owns |
//! |---|---|
//! | [`pci`] | BDF arithmetic, ECAM configuration-space reads, header and BAR decoding |
//! | [`id`] | the device table: device ids, generations, display versions, steppings, quirks, apertures |
//! | [`regs`] | the register table, the mapped window, typed 32-bit access and its ordering |
//! | [`pipe`] | reference section 11 phases 3.4 and 4: the timing registers, the DDB, the watermarks, the primary plane, and the pipe-side read-backs of phase 6 |
//! | [`probe`] | the walk, the identity decision, the register reads, and the report |
//! | [`debugfs`] | the report, exposed as a file in the DRM debug filesystem |
//!
//! The modeset is a second group: the memory it scans out of, the output it
//! scans through, and the sequence that ties them together.  The design is
//! `docs/design/intel-modeset.md`.
//!
//! | module | what it owns |
//! |---|---|
//! | [`gtt`] | the GGTT page table: the entry layout, a run of entries for a physical range, and the read-back that proves the run landed |
//! | [`fb`] | a framebuffer the display engine can read: geometry, the allocation, and the failure path |
//! | [`scanout`] | that framebuffer as the console's [`crate::pseudofs::dev::scanout::ScanoutSurface`], offered to [`crate::drm::screen`] |
//! | [`output`] | reference section 11 phase 5: the port PLL, the DDI, the transcoder, and the read-backs that say the DDI is alive |
//! | [`pattern`] | section 11 phase 6.5: the colour-bar test pattern written into a linear XRGB8888 framebuffer |
//! | [`modeset`] | section 11 phases 3 to 6 in one sequence: the mode choice, the pipe, the output, and the proof that the result is scanning out |
//!
//! ## What it proves, and what it does not
//!
//! A successful probe proves that the platform's configuration space was read,
//! that a device matching the table is present at a specific bus address, that
//! its register aperture was mapped, and that the registers answered with
//! values -- which is a different claim from "the values are correct", and a
//! much weaker one than "the display engine works".  No mode is set, no plane
//! is enabled, no pixel is scanned out.  On a machine that has never run this
//! code against a real Gen12 part, the register *values* in the report have
//! never been observed: the decoding around them has been tested against
//! synthetic configuration space, and that is the limit of what the tests say.
//!
//! ## Where the display driver plugs in
//!
//! Nothing here precludes a modeset driver, and two decisions are there to
//! make one possible:
//!
//! * The identity layer is the only place that knows what a device *is*
//!   ([`id::DisplayDevice`]), so a mode-setting driver asks it for the
//!   apertures, the stepping and the quirks instead of switching on a device
//!   id of its own.
//! * The register window is mapped once, here, and stays mapped in the kernel
//!   address space.  A driver that later owns the device reuses the same
//!   window through the same [`regs::RegisterWindow`] type.
//!
//! The scanout side is deliberately untouched: `pseudofs::dev::scanout`
//! defines the surface a framebuffer console consumes, and an Intel display
//! driver would implement it once it can program a pipe.  This module supplies
//! the device identity and register access that implementation will need, and
//! nothing else.
//!
//! ## After the boot: the hotplug watch
//!
//! [`probe_at_boot`] and [`bring_up_at_boot`] each run once and report what
//! they found.  A monitor plugged in afterwards is invisible to both, so this
//! module also owns the one thing that looks again: a task that sleeps, reads
//! the south display's live connect state, and re-runs the sink step for the
//! device when that state changes.  It is described in
//! `docs/design/intel-hotplug.md`; the short version is that the poll is two
//! register reads that write nothing, and every expensive step happens in task
//! context after the read.

mod clk;
mod connect;
pub(crate) mod debugfs;
pub(crate) mod fb;
mod gmbus;
pub(crate) mod gtt;
mod hpd;
mod id;
mod modeset;
mod output;
mod pattern;
mod pci;
mod phy;
mod pipe;
mod pll;
mod power;
mod probe;
mod regs;
pub(crate) mod scanout;
mod sink;
mod swing;
mod timing;

#[cfg(test)]
mod testbus;

use alloc::{string::String, vec::Vec};

use spin::Mutex;

/// The machine's own clock, which only the watch needs: the poll decides what
/// to do, and the machine's clock is what the log line is stamped with.
#[cfg(target_os = "none")]
use self::gmbus::MonotonicTimer;
use self::{
    gmbus::PollTimer,
    id::Aperture,
    pci::Bdf,
    probe::{BusFacts, ProbeReport, WindowStatus},
    regs::{PROBE_WINDOW, RegisterWindow, Registers},
};

/// The report of the one probe this kernel runs, kept for the debug file.
///
/// The probe runs once, at boot, and its findings do not change afterwards:
/// the device is either there or it is not, and re-reading registers on every
/// open of a debug file would make a diagnostic tool a source of hardware
/// traffic.
static REPORT: Mutex<Option<ProbeReport>> = Mutex::new(None);

/// What the power step of the bring-up order found, kept for the debug file.
///
/// `None` means either that the step did not run -- there was no identified
/// device, or its register window could not be mapped -- or that it ran and
/// failed, in which case [`POWER_FAILURE`] holds the reason.  The two are kept
/// apart on purpose: "the display was never powered" and "the display could not
/// be powered" are different facts and the log says which one happened.
static POWER: Mutex<Option<power::PowerState>> = Mutex::new(None);

/// Why the power step did not produce a state, when it was attempted and
/// failed.  Reference §11 phase 1: everything after this point is behind the
/// power wells, so a failure here is reported once, in full, rather than
/// re-derived by every later step that then fails for an unrelated-looking
/// reason.
static POWER_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// What the connector step of the bring-up order found, kept for the debug
/// file.
///
/// This is the connector the modeset will take: the pin a monitor answered on,
/// the DDI it carries, the validated EDID, the mode layer's plan and the live
/// hotplug state, plus what phase 2.1 found for the AUX/DDC power wells.  It is
/// the composition of reference §11 phases 2.1, 2.2 and 2.3, and it is kept for
/// the same reason the probe's report is: on a machine whose only console is
/// the screen, an EDID -- and the well that explains a bus which would not
/// produce one -- is worth being able to read twice.
static CONNECT: Mutex<Option<connect::ConnectReport>> = Mutex::new(None);

/// How often the after-boot hotplug watch reads the live connect state.
///
/// This number *is* the detection latency: a sleeping task notices a monitor
/// at the first poll after the electrical event, so the delay is between zero
/// and one interval (plus whatever scheduling delay the task sees -- nothing
/// here is a real-time guarantee).  Two hundred and fifty milliseconds is four
/// uncached reads a second of two always-on registers, against a delay nobody
/// watching a screen would notice.
///
/// The alternative considered and not built is a 100 Hz timer callback that
/// reads `SDEISR` in interrupt context and publishes an edge for a task to act
/// on.  It buys ten-millisecond resolution at a hundred times the register
/// traffic, and it adds state two contexts have to agree about, for a
/// difference no person can see.  The reasoning is written down in
/// `docs/design/intel-hotplug.md` rather than left in this comment alone,
/// because "why is it a task and not an interrupt" is the first question a
/// reader asks.
const HOTPLUG_POLL_INTERVAL: core::time::Duration = core::time::Duration::from_millis(250);

/// The most transitions the after-boot watch keeps for the debug file.
///
/// A connector whose cable makes intermittent contact can produce a transition
/// on every poll, so an unbounded list would be a way for a loose plug to
/// exhaust kernel memory.  The most recent [`HOTPLUG_EVENT_LIMIT`] transitions
/// are kept and the rest are counted, so the file shows both the tail of the
/// flap and the fact that there was more of it.  This is a bound on what is
/// *remembered*, not storm mitigation: the work each transition calls for
/// still happens, because the first thing a flapping connector needs is a log
/// of the flap (see the design note).
const HOTPLUG_EVENT_LIMIT: usize = 32;

/// What the after-boot hotplug watch has seen since bring-up, kept for the
/// debug file.
///
/// `None` means the watch never started, which is a different fact from "the
/// watch is running and has seen nothing"; the two are reported in different
/// words, because on a machine with no mapped register window the second would
/// be a claim this kernel cannot make.
static HOTPLUG: Mutex<Option<HotplugWatch>> = Mutex::new(None);

/// What the modeset observed, as the text a reader works from afterwards.
///
/// Kept beside the probe's report for the reason the probe's is kept: phase 6's
/// readings are the only evidence that the display engine is scanning this
/// kernel's framebuffer out, and on a machine whose console is the screen a
/// boot log scrolls away.  A run that did not happen, and a run that failed,
/// both leave their reason here rather than leaving the file silent about it.
static MODESET: Mutex<Option<String>> = Mutex::new(None);

/// What the graphics address space turned out to be, as text.
///
/// The aperture is read from the device (`ApertureSize`), not inferred from the
/// BAR split, and the number decides how much address space may be handed out
/// at all -- so it belongs next to the modeset in the file a person reads back,
/// not only in a boot log that has scrolled away.
static GTT: Mutex<Option<String>> = Mutex::new(None);

/// A value as grouped hexadecimal, the way a register dump is written down.
///
/// `0x0000_6000_0000_0000` can be read a field at a time; `0x600000000000`
/// cannot.  Rust's formatter has no digit-grouping flag, so the grouping is
/// done here rather than left out of every log line.
pub(crate) fn hex(value: u64, digits: usize) -> String {
    use alloc::format;

    let digits = digits.clamp(1, 16);
    let raw = format!("{value:0>digits$x}");
    let mut out = String::with_capacity(2 + raw.len() + raw.len() / 4);
    out.push_str("0x");
    for (index, digit) in raw.char_indices() {
        if index > 0 && (raw.len() - index) % 4 == 0 {
            out.push('_');
        }
        out.push(digit);
    }
    out
}

/// Run the Intel display probe once and print what it found.
///
/// The console is the only output device on the target machine, so the report
/// goes to the log as it is produced: a person who was not watching the boot
/// can read the same text back from [`debugfs`] afterwards.
///
/// The probe runs first and alone; the bring-up order's later steps are run
/// from [`bring_up_at_boot`], which needs the window this produces.
pub(crate) fn probe_at_boot() {
    let report = platform_probe();
    report.log();
    *REPORT.lock() = Some(report);
}

/// Run the rest of the bring-up order against the device the probe found.
///
/// This is reference §11 phases 1 and 2, in the order the reference gives them,
/// and each step is attempted only on a device whose register window the probe
/// mapped -- a device this kernel has a model for.  On a machine with no Intel
/// display, or one whose window could not be mapped, nothing here runs and it
/// says so rather than reporting a half-run sequence as a result.
///
/// The order is the point.  Power comes before the DDC pin pair because GMBUS
/// is a channel behind the AUX/DDC power well and a well cannot be requested
/// before `PW_1` is up; the well comes before the EDID read because a channel
/// behind a shut gate NAKs every address, which is indistinguishable from an
/// empty port; the sink comes before any mode because a timing computed from a
/// guessed EDID is worse than no timing at all.  Each step logs as it goes, for
/// the same reason the probe does.
pub(crate) fn bring_up_at_boot() {
    let windows = mapped_windows();
    if windows.is_empty() {
        axlog::info!(
            "intel-gpu: no device with a mapped register window, so the power and connector steps \
             of the bring-up order did not run"
        );
        return;
    }

    // The devices whose phase-1 power came up, which are the only ones later
    // steps may touch.
    let mut powered = Vec::new();
    for (bdf, window) in &windows {
        axlog::info!("intel-gpu: powering up {bdf} (reference section 11 phase 1)");
        match power::bring_up(window) {
            Ok(state) => {
                state.log();
                *POWER.lock() = Some(state);
                powered.push((*bdf, *window));
            }
            Err(error) => {
                // One line, once, and the sequence stops here: §11 phase 1 is
                // the gate everything else is behind, and a later step that
                // fails because the display is unpowered would be reported as
                // its own fault rather than this one's.
                let text = alloc::format!("intel-gpu: power: {} ({error:?})", error.describe());
                axlog::warn!("{text}");
                *POWER_FAILURE.lock() = Some(text);
                continue;
            }
        }
    }

    // Phase 2 runs for every mapped device whose power came up, because the
    // connector is per-device and one device's monitor must not be lost to
    // another device's failure.  `resolve_at_boot` is phases 2.1, 2.2 and 2.3
    // composed: the AUX/DDC power wells first, then one pass over the bus, then
    // the connector the modeset takes.
    let report = { REPORT.lock().clone() };
    if let Some(report) = report {
        let connect = connect::resolve_at_boot(&report);
        *CONNECT.lock() = Some(connect);
    }

    // The modeset runs before the watch starts, and that order is deliberate:
    // a watch that was already polling could see a monitor arrive while the
    // only sequence this kernel has for programming a pipe is halfway through
    // it, and there is no second modeset to give it.
    #[cfg(target_os = "none")]
    modeset_at_boot(&powered);

    // The after-boot watch starts here and nowhere else.  Every step it depends
    // on has now run: the window is mapped, the display is powered, hotplug
    // detection is enabled, and the states the boot step read are in [`CONNECT`]
    // to be the baseline.  A machine that reached none of that never gets here
    // and never polls.
    #[cfg(target_os = "none")]
    start_hotplug_watch(&powered);
}

/// Ask the display engine to scan a pattern out, and offer the result to the
/// console.
///
/// This is reference §11 phases 3.2 to 6 as one boot-time step, and the only
/// caller of [`modeset::set_mode`] in the product build.  Everything it needs
/// has already happened by the time it runs: the probe mapped the register
/// window, phase 1 powered the display, phase 2 found a monitor and produced
/// the mode layer's plan.  It takes the first device a monitor answered on
/// whose power came up -- there is one display engine and one console, so a
/// second monitor has nowhere to go.
///
/// Two of its inputs are worth naming because they are what the machine
/// supplies and this kernel cannot:
///
/// * **The surface's size.**  [`modeset::choose_mode`] runs inside `set_mode`,
///   so the framebuffer has to be allocated before the mode is known.  It is
///   allocated for the larger of the mode layer's choice and §11 phase 3.1's
///   1920x1080@60 preference, which are the only two modes `choose_mode` can
///   return, so whichever it picks fits.  A larger monitor therefore costs the
///   larger allocation, and a frame that cannot be allocated is refused by name
///   with the firmware's console still up.
/// * **§8.5's voltage-swing values.**  They are the board's, not ours, so they
///   are read back out of the PHY the firmware programmed ([`swing`]).  When
///   that read refuses -- a port the firmware never brought up -- the request
///   carries no values and [`output::OutputProgram::plan`] refuses with
///   `MissingBufferTranslation` *before the first write*, which is the same
///   outcome with one error path instead of two.
///
/// The console is offered the surface only through [`scanout::register`], which
/// re-checks the phase-6 verdict against the surface it was handed: on anything
/// but a proven scanout the firmware framebuffer keeps the screen and the
/// reason the Intel one did not is what the log carries.  The ordering that
/// makes the offer count is the entry point's: `drm::init_virtio_gpu` runs
/// before `pseudofs::mount_all`, and `/dev/fb0` asks for a surface only when
/// that filesystem is built, so the candidate registered here is in the list
/// before the console consults it.  Nothing here retries,
/// nothing unwinds, and nothing panics -- a half-programmed mode is not a mode,
/// and on a machine whose only output is the screen the failure's own signature
/// is the diagnostic.
#[cfg(target_os = "none")]
fn modeset_at_boot(powered: &[(pci::Bdf, RegisterWindow)]) {
    use alloc::sync::Arc;

    // The report is moved out rather than cloned: `ModePlan` is not `Clone` and
    // holding the lock across an allocation and a modeset would be worse than
    // either.  It is put back before this function returns, so the debug file
    // still shows what phase 2 found.
    let Some(report) = CONNECT.lock().take() else {
        return;
    };
    let Some(connector) = report.connectors.first() else {
        axlog::info!(
            "intel-modeset: no monitor answered on any DDC pin, so no mode is set and the \
             firmware's framebuffer keeps the console (reference section 11 phase 3.1)"
        );
        *CONNECT.lock() = Some(report);
        return;
    };
    let Some((window, aperture, physical)) = mapped_facts(connector.bdf) else {
        axlog::warn!(
            "intel-modeset: display {} has a connector but no mapped register window, so nothing \
             was programmed",
            connector.bdf
        );
        *CONNECT.lock() = Some(report);
        return;
    };
    if !powered.iter().any(|(bdf, _)| *bdf == connector.bdf) {
        axlog::warn!(
            "intel-modeset: display {} never came up in phase 1, so it is not programmed; the \
             power failure above is the finding, not this line",
            connector.bdf
        );
        *CONNECT.lock() = Some(report);
        return;
    }
    if aperture.bar != 0 {
        axlog::warn!(
            "intel-modeset: the window mapped for {} is {} (BAR {}), not the GTTMMADR aperture \
             the graphics address space lives in, so nothing was programmed",
            connector.bdf,
            aperture.name,
            aperture.bar
        );
        *CONNECT.lock() = Some(report);
        return;
    }
    // The aperture's size is the model's, not a probe: §11 phase 0.4 says to
    // read the actual BAR sizes and this probe is read-only, so it cannot use
    // the write-all-ones trick.  A BAR the firmware shrank would fail the page
    // table's own read-back check rather than silently addressing past it.
    let Some(bar0_len) = aperture.size else {
        axlog::warn!(
            "intel-modeset: no documented size for {}, so the GTT array is not mapped and no mode \
             is set",
            aperture.name
        );
        *CONNECT.lock() = Some(report);
        return;
    };

    // The aperture the allocator may use is the device's own statement of it:
    // the BAR split says how much *window* there is, and i915 takes the size
    // from the `GGMS` field instead (`gt/intel_ggtt.c:1228`, decoded by
    // `gen8_get_total_gtt_size` at `:1107-1121`).  A machine where the field
    // cannot be read keeps the window-derived size, with the observation
    // visibly absent rather than assumed.
    let aperture_size = match pci::Ecam::platform() {
        Some(ecam) => gtt::ApertureSize::read(&ecam, connector.bdf),
        None => gtt::ApertureSize::NotObserved,
    };
    let gtt = match gtt::Gtt::map(physical, bar0_len, aperture_size) {
        Ok(gtt) => gtt,
        Err(error) => {
            axlog::warn!("intel-modeset: {}", error.describe());
            *CONNECT.lock() = Some(report);
            return;
        }
    };
    *GTT.lock() = Some(gtt.describe());

    // The two modes `choose_mode` can return are the mode layer's choice and
    // the reference timing, so a surface that covers both covers the choice.
    let chosen = connector.plan.selection.mode;
    let width = u32::from(chosen.hdisplay).max(u32::from(modeset::REFERENCE_HDISPLAY));
    let height = u32::from(chosen.vdisplay).max(u32::from(modeset::REFERENCE_VDISPLAY));
    axlog::info!(
        "intel-modeset: allocating {width}x{height} XRGB8888 for {} (phase 3.2), then programming \
         pipe A through DDI {} (phases 3.4 to 5.7)",
        connector.bdf,
        connector.ddi
    );
    let surface = match fb::Surface::allocate(&gtt, width, height, fb::Format::Xrgb8888) {
        Ok(surface) => surface,
        Err(error) => {
            axlog::warn!("intel-modeset: {}", error.describe());
            *CONNECT.lock() = Some(report);
            return;
        }
    };

    // The blocks the mode layer's plan was made from, in the order it read them.
    let mut edid = Vec::with_capacity(2 * gmbus::EDID_BLOCK_LEN);
    edid.extend_from_slice(connector.edid.as_slice());
    if let Some(extension) = &connector.extension {
        edid.extend_from_slice(extension.as_slice());
    }

    let swing = match swing::read_firmware_swing(&window, connector.ddi) {
        Ok(swing) => Some(swing),
        Err(source) => {
            // Not fatal here: the request below carries no values, and phase
            // 5's own refusal names the table and the reference's gap.
            axlog::warn!(
                "intel-modeset: {} has no buffer-translation values to replay: {}",
                connector.ddi,
                source.describe()
            );
            None
        }
    };

    let mut request = modeset::ModeRequest::new(
        connector.ddi,
        pipe::Pipe::A,
        &connector.plan,
        &edid,
        &surface,
        // The named field encoding is the one the ADL-N path writes and reads
        // back (`icl_wrpll_params_populate`); the Skylake codes are the other
        // convention and not this part's.
        pll::PllFieldEncoding::Named,
    );
    request.frame = 0;
    request.swing = swing;
    // §8.6 gives no sourced HDMI encoding for PHY_LINK_RATE; zero is the
    // honest value until a dump from this machine settles it (reference §13.4).
    request.link_rate = output::LinkRate::NoSourcedEncoding;

    let outcome = match modeset::set_mode(&window, &gmbus::MonotonicTimer, &request) {
        Ok(outcome) => outcome,
        Err(error) => {
            let text = alloc::format!(
                "intel-modeset: the mode was not set: {} ({error:?}).  The firmware's framebuffer \
                 keeps the console (reference section 11 phase 6)",
                error.describe()
            );
            axlog::warn!("{text}");
            *MODESET.lock() = Some(text);
            *CONNECT.lock() = Some(report);
            return;
        }
    };
    outcome.log();
    *MODESET.lock() = Some(outcome.render());

    let verdict = if outcome.prove.verdict().is_scanning_out() {
        match outcome.prove.surflive() {
            Some(surflive) => scanout::Verdict::Scanning { surflive },
            // A scanning verdict without the reading it was made from is not
            // evidence, and the console is not moved on anything less.
            None => scanout::Verdict::NotScanning {
                reason: String::from(
                    "phase 6 reports the pipe scanning, but the PLANE_SURFLIVE reading that \
                     verdict rests on is absent",
                ),
            },
        }
    } else {
        scanout::Verdict::NotScanning {
            reason: outcome
                .prove
                .verdict()
                .unavailable_reason()
                .unwrap_or_else(|| {
                    String::from("phase 6 did not prove the pipe is scanning this surface out")
                }),
        }
    };
    scanout::register(Arc::new(surface), verdict);
    *CONNECT.lock() = Some(report);
}

/// The mapped window, the aperture the probe mapped it from, and the BAR's
/// physical base, for one device.
///
/// Read back out of the probe's report rather than carried along: the report is
/// what the debug file shows a reader, so a device the probe refused to map
/// cannot be programmed by a later step that kept its own copy of the facts.
#[cfg(target_os = "none")]
fn mapped_facts(bdf: pci::Bdf) -> Option<(RegisterWindow, &'static Aperture, u64)> {
    let report = REPORT.lock();
    let report = report.as_ref()?;
    for found in &report.displays {
        if found.info.bdf != bdf {
            continue;
        }
        if let WindowStatus::Mapped {
            window,
            aperture,
            physical,
        } = found.status
        {
            return Some((window, aperture, physical));
        }
    }
    None
}

/// The devices whose register window the probe mapped, with those windows.
///
/// A device the probe found but could not map is skipped here rather than
/// guessed at, which is the policy the probe itself applies to a part it has no
/// model for.
fn mapped_windows() -> alloc::vec::Vec<(pci::Bdf, RegisterWindow)> {
    let report = REPORT.lock();
    let Some(report) = report.as_ref() else {
        return alloc::vec::Vec::new();
    };
    report
        .displays
        .iter()
        .filter_map(|found| match found.status {
            WindowStatus::Mapped { window, .. } => Some((found.info.bdf, window)),
            WindowStatus::Refused(_) => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The after-boot hotplug watch.
//
// Everything below this line runs *after* the bring-up order, and the whole of
// its read-and-decide half is a plain function so that a host test can drive
// the poll, the edge compare and the re-probe through a model of the
// controller.  Only the thread is a thread, and only the thread is
// `target_os = "none"`.
//
// Nothing here touches a register on the poll path.  The writes this path does
// make are the re-probe's, and they are phase 2's own: `HPD_ENABLE`, already
// set, and the GMBUS controller's control registers.  The poll itself writes
// nothing at all -- `SDEISR` is write-one-to-clear, so a poll that wrote it
// would destroy the state it came to read.
// ---------------------------------------------------------------------------

/// One transition the watch saw, and the millisecond it was seen at.
struct HotplugEvent {
    /// Milliseconds since the platform's monotonic clock started.  A number
    /// rather than a formatted duration, because the log line is written where
    /// the clock is read and the file is rendered much later.
    millis: u64,
    transition: hpd::DdiTransition,
}

/// What the after-boot watch has seen, for the debug file.
struct HotplugWatch {
    /// The device whose register window is polled.
    ///
    /// One watch follows one device.  A machine with two display devices this
    /// kernel had mapped would get a watch for the first, and the log says so
    /// when that happens: two watches would need a report with a heading per
    /// device, and the target machine has one display function.
    bdf: Bdf,
    /// The transitions, oldest first, at most [`HOTPLUG_EVENT_LIMIT`] of them.
    events: Vec<HotplugEvent>,
    /// How many transitions were dropped off the front of that list.
    dropped: u64,
    /// What the phase-2 probe re-run after the most recent transition found.
    /// `None` until a transition has happened.
    last_probe: Option<sink::DeviceSink>,
}

impl HotplugWatch {
    fn new(bdf: Bdf) -> Self {
        Self {
            bdf,
            events: Vec::new(),
            dropped: 0,
            last_probe: None,
        }
    }

    /// Take one pass: keep what changed, and keep the re-probe it called for.
    ///
    /// The logging is the caller's and happens before this, outside the lock:
    /// a console write has no business happening while a lock a debug-file read
    /// also wants is held.
    fn record(&mut self, millis: u64, pass: ReconcilePass) {
        for transition in pass.transitions.iter().flatten() {
            if self.events.len() == HOTPLUG_EVENT_LIMIT {
                self.events.remove(0);
                self.dropped += 1;
            }
            self.events.push(HotplugEvent {
                millis,
                transition: *transition,
            });
        }
        // A pass produces a re-probe exactly when it produces a transition, so
        // this is the same condition -- but it is written as its own so that a
        // pass with no probe cannot silently clear the last one.
        if let Some(device) = pass.probe {
            self.last_probe = Some(device);
        }
    }

    /// The lines the debug file carries.
    fn render(&self) -> String {
        let mut out = alloc::format!(
            "\n--- hotplug after boot (reference section 11 phase 2.2) ---\nwatching {}: one \
             SDEISR read every {} ms.  The poll writes no register; a transition re-runs the \
             phase-2 sink probe.\n",
            self.bdf,
            HOTPLUG_POLL_INTERVAL.as_millis(),
        );
        if self.events.is_empty() {
            out.push_str("  no transition since the state the sink step read at boot\n");
        }
        for event in &self.events {
            out.push_str(&alloc::format!(
                "  {} ms: {}\n",
                event.millis,
                event.transition.describe()
            ));
        }
        if self.dropped != 0 {
            out.push_str(&alloc::format!(
                "  {} older transitions are not listed: this file keeps the most recent {}\n",
                self.dropped,
                HOTPLUG_EVENT_LIMIT,
            ));
        }
        if let Some(device) = &self.last_probe {
            out.push_str("  the phase-2 sink probe re-run after the last transition:\n");
            device.render_into(&mut out);
        }
        out
    }
}

/// What one pass of the watch found.
struct ReconcilePass {
    /// The register that refused, when one did.  Never set together with a
    /// transition: a state that could not be read is not a state that changed.
    failure: Option<hpd::HpdError>,
    /// The transitions this pass produced, at most one per DDI.
    transitions: [Option<hpd::DdiTransition>; 4],
    /// The phase-2 sink probe, when a transition called for one.
    probe: Option<sink::DeviceSink>,
}

impl ReconcilePass {
    /// How many transitions this pass produced.
    fn changed(&self) -> usize {
        self.transitions.iter().flatten().count()
    }
}

/// Read the live connect state once, and do what a change calls for.
///
/// This is the whole of the watch that is not a thread, and the order is the
/// point:
///
/// 1. One pair of register reads, writing nothing ([`hpd::poll_connect`]).
/// 2. An edge compare against the state the last answered poll left
///    ([`hpd::ConnectTracker`]).  A register that did not answer produces
///    neither an event nor a re-probe: "I could not look" is not "nothing is
///    there".
/// 3. Only when something changed, the phase-2 sink probe for this device, in
///    the context of whatever task called this.  That is why the watch is a
///    sleeping task and not a timer callback: the step that follows a change
///    waits on GMBUS and allocates, and neither belongs in an interrupt.
///
/// Step 3 is the *same* composition bring-up runs, deliberately.  A monitor
/// found after boot is probed by the code that probed the one found at boot, so
/// the two results are comparable and there is one implementation of "find out
/// what is on this port" instead of two that drift apart.  It reads the EDID
/// and it writes what phase 2 writes -- `HPD_ENABLE`, already set, and the
/// GMBUS controller's control registers -- which is not the poll path: the
/// poll path is step 1, and step 1 wrote nothing.
///
/// The re-probe is the *last* thing a pass does, so the transitions a pass
/// found are logged before the bus work starts and a slow GMBUS transaction
/// delays the next poll rather than the report of what was seen.
fn reconcile_once<R: Registers, T: PollTimer>(
    bdf: Bdf,
    regs: &R,
    timer: &T,
    tracker: &mut hpd::ConnectTracker,
) -> ReconcilePass {
    let poll = hpd::poll_connect(regs);
    let mut transitions = [None; 4];
    let mut changed = 0;
    for state in poll.states().into_iter().flatten() {
        let Some(transition) = tracker.observe(state) else {
            continue;
        };
        // One poll produces at most one state per DDI and each state at most
        // one transition, so `changed` cannot reach the end of this array.  The
        // guard is here anyway: a hotplug is no place for an index panic, and
        // what it would drop is one more line in a report, not a decision.
        let Some(slot) = transitions.get_mut(changed) else {
            break;
        };
        *slot = Some(transition);
        changed += 1;
    }
    let probe = if changed == 0 {
        None
    } else {
        Some(sink::probe_one(bdf, regs, timer))
    };
    ReconcilePass {
        failure: poll.failure(),
        transitions,
        probe,
    }
}

/// The baseline the watch starts from: what the phase-2 pass already reported.
///
/// A watch that reported the state it found on its first poll would be
/// reporting its own first look as a hotplug.  The connector step read every
/// DDI once and logged each one, so that reading is the baseline and only a
/// change from it is an event.  A DDI the boot read could not answer for gets
/// no baseline, and its first answered poll is adopted in silence -- see
/// [`hpd::ConnectTracker`], which is where that rule and its reason live.
fn boot_baseline(report: &connect::ConnectReport, bdf: Bdf) -> hpd::ConnectTracker {
    let mut tracker = hpd::ConnectTracker::new();
    for (device, statuses) in &report.hotplug {
        if *device != bdf {
            continue;
        }
        for status in statuses {
            tracker.seed(status.ddi, status.effective_connect());
        }
    }
    tracker
}

/// Start the after-boot hotplug watch, once, for the first device phase 1
/// brought up.
///
/// The call site is the end of [`bring_up_at_boot`], and the ordering is the
/// whole of the safety argument for polling: by the time this runs the register
/// window is mapped, the display is powered, hotplug detection is enabled, and
/// the states the sink step read are there to be the baseline.  On a machine
/// with no mapped window [`bring_up_at_boot`] has already returned and nothing
/// polls at all.
#[cfg(target_os = "none")]
fn start_hotplug_watch(powered: &[(Bdf, RegisterWindow)]) {
    use core::sync::atomic::{AtomicBool, Ordering};

    /// Set once, so that a second call cannot start a second watch: two watches
    /// would each see the same edge and log it twice.
    static STARTED: AtomicBool = AtomicBool::new(false);

    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some((bdf, window)) = powered.first().copied() else {
        axlog::info!(
            "intel-hpd: no display device reached phase 1, so nothing polls the hotplug status \
             registers after boot"
        );
        return;
    };
    if powered.len() > 1 {
        axlog::info!(
            "intel-hpd: {} display devices came up; the after-boot hotplug watch follows {bdf} \
             only",
            powered.len()
        );
    }
    // The baseline is taken before the task starts, and the lock is released
    // before anything that allocates.
    let tracker = match &*CONNECT.lock() {
        Some(report) => boot_baseline(report, bdf),
        None => hpd::ConnectTracker::new(),
    };
    // Published before the task starts, so the debug file says "watching and
    // nothing yet" rather than "the watch never started" in the window between
    // the spawn and the first poll.
    *HOTPLUG.lock() = Some(HotplugWatch::new(bdf));
    let name = String::from("intel-hotplug");
    match axtask::spawn_with_name(move || watch_hotplug(bdf, window, tracker), name) {
        Ok(_) => axlog::info!(
            "intel-hpd: watching {bdf} for hotplug: one SDEISR read every {} ms, and the poll \
             itself writes no register",
            HOTPLUG_POLL_INTERVAL.as_millis()
        ),
        Err(error) => {
            // Put the state back the way it was: a watch that did not start
            // must not leave a file claiming one is running.
            STARTED.store(false, Ordering::Release);
            *HOTPLUG.lock() = None;
            axlog::warn!(
                "intel-hpd: the after-boot hotplug watch could not start, so a monitor plugged in \
                 after boot will not be noticed: {error:?}"
            );
        }
    }
}

/// Follow one display device's hotplug state for the rest of the boot.
///
/// A task that sleeps, polls and then does the work a change calls for in its
/// own context.  Everything expensive or blocking -- the sleep, the GMBUS
/// transactions of the re-probe, the allocation a report line needs -- is on
/// this side of the boundary, which is the reason the watch is a task rather
/// than a timer callback.
#[cfg(target_os = "none")]
fn watch_hotplug(bdf: Bdf, window: RegisterWindow, mut tracker: hpd::ConnectTracker) {
    // The failure reported last time, so that a window which does not reach
    // `SDEISR` is reported once rather than four times a second.
    let mut reported: Option<hpd::HpdError> = None;
    loop {
        if axtask::sleep(HOTPLUG_POLL_INTERVAL).is_err() {
            // A timer admission that is temporarily exhausted must not stop the
            // watch; yielding keeps the task live and keeps the loop from
            // becoming a tight poll, exactly as the PCI input reconcile worker
            // does.
            axtask::yield_now();
        }
        let pass = reconcile_once(bdf, &window, &MonotonicTimer, &mut tracker);
        match pass.failure {
            Some(failure) if reported != Some(failure) => {
                axlog::warn!(
                    "intel-hpd: the after-boot watch cannot read the live connect state, so it \
                     cannot see a monitor arrive: {}",
                    failure.describe()
                );
                reported = Some(failure);
            }
            // The same refusal again: already said, and saying it every poll
            // would fill the log with one line.
            Some(_) => {}
            None => reported = None,
        }
        if pass.changed() == 0 {
            continue;
        }
        // The clock is read, and the lock is taken, only once there is
        // something to record.  The lines go to the log before the lock: the
        // raw words are what a person sees as it happens, and a console write
        // holds nothing.
        let millis = axhal::time::monotonic_time_nanos() / 1_000_000;
        for transition in pass.transitions.iter().flatten() {
            axlog::info!(
                "intel-hpd: hotplug at {millis} ms: {}",
                transition.describe()
            );
        }
        let mut slot = HOTPLUG.lock();
        let watch = slot.get_or_insert_with(|| HotplugWatch::new(bdf));
        watch.record(millis, pass);
    }
}

/// The report of the boot probe as text.
pub(crate) fn report_text() -> String {
    let mut text = match &*REPORT.lock() {
        Some(report) => report.render(),
        None => String::from(
            "intel-gpu: the Intel display probe has not run in this boot; it runs from the DRM \
             initialization path, so this file is empty only if that path never executed\n",
        ),
    };
    // The locks are taken one at a time: nothing holds two at once.
    if let Some(failure) = &*POWER_FAILURE.lock() {
        text.push_str(failure);
        text.push('\n');
    }
    if let Some(state) = &*POWER.lock() {
        text.push_str(&state.render());
    }
    if let Some(connect) = &*CONNECT.lock() {
        text.push_str(&connect.render());
    }
    if let Some(gtt) = &*GTT.lock() {
        text.push_str(gtt);
    }
    if let Some(modeset) = &*MODESET.lock() {
        text.push_str(modeset);
    }
    if let Some(watch) = &*HOTPLUG.lock() {
        text.push_str(&watch.render());
    }
    text
}

/// Whether the boot probe found a display device it could identify.
pub(crate) fn identified_device() -> Option<&'static id::DisplayDevice> {
    let report = REPORT.lock();
    let report = report.as_ref()?;
    report
        .displays
        .iter()
        .find_map(|found| found.identity.device())
}

/// Run the probe against the platform's PCI bus.
#[cfg(target_os = "none")]
fn platform_probe() -> ProbeReport {
    let Some(ecam) = pci::Ecam::platform() else {
        return ProbeReport::unavailable(
            "this build declares no ECAM aperture, so PCI configuration space cannot be reached",
            BusFacts {
                ecam_base: None,
                bus_end: 0,
            },
        );
    };
    let facts = BusFacts {
        ecam_base: Some(ecam.base()),
        bus_end: ecam.bus_end(),
    };
    probe::run(&ecam, facts, open_register_window)
}

/// The probe cannot run in a hosted test build, and says so rather than
/// reporting an empty bus, which would be indistinguishable from a machine
/// with no GPU.
#[cfg(not(target_os = "none"))]
fn platform_probe() -> ProbeReport {
    ProbeReport::unavailable(
        "hosted test build: PCI configuration space and device memory are not reachable here, so \
         the probe did not run and this report describes nothing on this machine",
        BusFacts {
            ecam_base: None,
            bus_end: 0,
        },
    )
}

/// Map the register aperture of a device the table has a model for.
///
/// This is the only place the probe turns an address from configuration space
/// into memory it can read, so it is where every refusal belongs: a device
/// with no memory BAR where the table puts the registers, a BAR that decodes
/// no address, and a BAR whose address contradicts the modelled aperture size
/// are all refused rather than mapped and interpreted.
#[cfg(target_os = "none")]
fn open_register_window(info: &pci::DeviceInfo, aperture: &'static Aperture) -> WindowStatus {
    let Some(bar) = info.memory_bar(aperture.bar) else {
        return WindowStatus::Refused(
            "the device declares no memory BAR where the device table puts its registers",
        );
    };
    let Some(address) = bar.address() else {
        return WindowStatus::Refused("the register BAR decodes no address");
    };
    if !aperture.admits_base(address) {
        return WindowStatus::Refused(
            "the register BAR address contradicts the aperture size this kernel models, so it \
             will not map on top of it",
        );
    }
    match RegisterWindow::map(address, PROBE_WINDOW) {
        Ok(window) => WindowStatus::Mapped {
            window,
            aperture,
            physical: address,
        },
        Err(_) => WindowStatus::Refused("mapping the register aperture failed"),
    }
}

#[cfg(test)]
mod tests {
    use alloc::{format, vec};

    use super::*;
    use crate::{
        drm::intel::{
            gmbus::{
                Pin,
                tests::{FakeClock, FakeController, valid_edid},
            },
            hpd::Ddi,
            regs::SDEISR,
        },
        test_support::scheduler_test_context,
    };

    /// A `Bdf` for a display function, for the report lines.
    fn bdf() -> Bdf {
        Bdf::new(0, 2, 0)
    }

    /// A controller with a monitor wired to `pin`, and nothing else attached.
    fn controller_with_monitor_on(pin: Pin) -> FakeController {
        FakeController::with_monitor(pin, &valid_edid(0))
    }

    #[test]
    fn hexadecimal_is_grouped_the_way_register_dumps_are() {
        assert_eq!(hex(0x0000_000c, 8), "0x0000_000c");
        assert_eq!(hex(0x6000_0000_0000, 16), "0x0000_6000_0000_0000");
        assert_eq!(hex(0xffff_ffff, 8), "0xffff_ffff");
        assert_eq!(hex(0, 8), "0x0000_0000");
        assert_eq!(hex(0, 16), "0x0000_0000_0000_0000");
        assert_eq!(hex(0x300, 4), "0x0300");
        assert_eq!(hex(0x46d0, 4), "0x46d0");
        // A request for more digits than a value has still produces a
        // fixed-width field, which is what makes a column of them line up.
        assert_eq!(hex(0x46d0, 8), "0x0000_46d0");
        // And a request for fewer digits than the value needs does not
        // truncate it.
        assert_eq!(hex(0xdead_beef, 4), "0xdead_beef");
    }

    /// The baseline a watch starts from, for a controller the test owns: the
    /// states the sink step reported at boot, in the shape the connector step
    /// keeps them in.
    fn boot_report(controller: &FakeController) -> connect::ConnectReport {
        let device = sink::probe_one(bdf(), controller, &FakeClock::new());
        connect::ConnectReport {
            hotplug: vec![(device.bdf, device.hotplug.clone())],
            ..connect::ConnectReport::default()
        }
    }

    #[test]
    fn the_baseline_is_the_state_the_boot_step_already_reported() {
        let _guard = scheduler_test_context();
        let controller = controller_with_monitor_on(Pin::DdiC);
        controller.set_word(SDEISR, Ddi::C.live_bit());
        let mut tracker = boot_baseline(&boot_report(&controller), bdf());

        // The boot step read DDI C's live bit and logged it, so the first poll
        // finds the same state and has nothing to report: a watch that reported
        // its own first look would say a monitor arrived that was there all
        // along.
        let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
        assert_eq!(pass.changed(), 0);
        assert!(pass.probe.is_none(), "no change, no re-probe");
        assert!(pass.failure.is_none());

        // And the baseline is DDI C's state, not "everything connected":
        // unplugging the word alone is a transition on DDI C.
        controller.set_word(SDEISR, 0);
        let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
        let transition = pass
            .transitions
            .iter()
            .flatten()
            .next()
            .expect("a transition");
        assert_eq!(transition.ddi, Ddi::C);
        assert!(transition.from);
        assert!(!transition.state.connected);
    }

    #[test]
    fn a_monitor_plugged_in_after_boot_is_seen_and_the_sink_is_probed_again() {
        let _guard = scheduler_test_context();
        // Nothing is attached, and the status word agrees.  The monitor's EEPROM
        // image is loaded from the start, so "plugged in" is only the pin being
        // wired up and the connect bit being set.
        let controller = controller_with_monitor_on(Pin::DdiB);
        controller.detach();
        let mut tracker = boot_baseline(&boot_report(&controller), bdf());
        let mut watch = HotplugWatch::new(bdf());

        // A steady state is not an event, and, more than that, it costs the bus
        // nothing: no GMBUS transaction is started by a poll that found no
        // change.
        let transactions = controller.transactions();
        let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
        assert_eq!(pass.changed(), 0);
        assert!(pass.probe.is_none());
        assert_eq!(controller.transactions(), transactions);

        // A monitor arrives: the sink answers on DDI B's DDC pin, and the live
        // connect bit for DDI B is set.
        controller.attach(Pin::DdiB);
        controller.set_word(SDEISR, Ddi::B.live_bit());
        let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
        assert_eq!(pass.changed(), 1);
        let transition = pass.transitions.iter().flatten().next().unwrap();
        assert_eq!(transition.ddi, Ddi::B);
        assert!(!transition.from);
        let probe = pass.probe.as_ref().expect("a change re-probes the sink");
        assert_eq!(probe.monitor, Some(Pin::DdiB));

        watch.record(1234, pass);
        let text = watch.render();
        // The report a reader gets has the transition, the raw words it came
        // from, and the phase-2 result behind it.
        assert!(text.contains("hotplug after boot"), "{text}");
        assert!(text.contains(&format!("{} ms:", 1234)), "{text}");
        assert!(text.contains("DDI B: disconnected -> connected"), "{text}");
        assert!(
            text.contains(&format!("SDEISR {:#010x}", Ddi::B.live_bit())),
            "{text}"
        );
        assert!(text.contains("SOUTH_CHICKEN1 0x00000000"), "{text}");
        assert!(
            text.contains("the phase-2 sink probe re-run after the last transition"),
            "{text}"
        );
        assert!(text.contains("monitor on pin 2"), "{text}");

        // And the same poll again is not an event, which is the whole of "the
        // same read twice is not a hotplug".
        let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
        assert_eq!(pass.changed(), 0);

        // Unplugged: the other edge, probed again, and the report says the sink
        // is gone.
        controller.detach();
        controller.set_word(SDEISR, 0);
        let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
        assert_eq!(pass.changed(), 1);
        let transition = pass.transitions.iter().flatten().next().unwrap();
        assert!(transition.from);
        assert!(!transition.state.connected);
        assert!(pass.probe.as_ref().unwrap().monitor.is_none());
        watch.record(2000, pass);
        let text = watch.render();
        assert!(text.contains("DDI B: connected -> disconnected"), "{text}");
        assert!(text.contains("no monitor"), "{text}");
        // The log line for the first transition is still there: the file keeps
        // the history, not only the last state.
        assert!(text.contains("DDI B: disconnected -> connected"), "{text}");
    }

    #[test]
    fn a_poll_that_cannot_read_the_registers_does_nothing_and_says_why_once() {
        let _guard = scheduler_test_context();
        let controller = FakeController::bare();
        controller.detach();
        controller.set_window_len(0x1000);
        let mut tracker = boot_baseline(&boot_report(&controller), bdf());
        let mut watch = HotplugWatch::new(bdf());

        // The window does not reach the hotplug block: no state, no event, no
        // re-probe, and a reason named by register.
        let transactions = controller.transactions();
        let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
        assert_eq!(pass.changed(), 0);
        assert!(pass.probe.is_none());
        assert_eq!(controller.transactions(), transactions);
        let failure = pass.failure.expect("a read that failed must be named");
        assert!(
            failure.describe().contains("SDEISR"),
            "{}",
            failure.describe()
        );

        // Nothing is recorded, so the file says the watch is running and has
        // seen nothing -- not that a monitor came or went.
        assert!(pass.probe.is_none());
        watch.record(0, pass);
        let text = watch.render();
        assert!(text.contains("no transition"), "{text}");
        assert!(!text.contains(" -> "), "{text}");
    }

    #[test]
    fn the_event_list_is_bounded_and_counts_what_it_dropped() {
        let _guard = scheduler_test_context();
        let controller = FakeController::bare();
        controller.detach();
        let mut tracker = boot_baseline(&boot_report(&controller), bdf());
        let mut watch = HotplugWatch::new(bdf());

        // A connector that makes and breaks contact on every poll.  What is
        // kept is bounded; what is counted is not, so the file says both what
        // happened last and that there was more of it.
        for step in 0..(HOTPLUG_EVENT_LIMIT + 3) {
            let connected = step % 2 == 0;
            controller.set_word(SDEISR, if connected { Ddi::B.live_bit() } else { 0 });
            let pass = reconcile_once(bdf(), &controller, &FakeClock::new(), &mut tracker);
            assert_eq!(pass.changed(), 1, "step {step}");
            watch.record(step as u64, pass);
        }
        assert_eq!(watch.events.len(), HOTPLUG_EVENT_LIMIT);
        assert_eq!(watch.dropped, 3);
        let text = watch.render();
        assert!(
            text.contains("3 older transitions are not listed"),
            "{text}"
        );
        // The most recent one is kept, so the file ends on the current state.
        assert!(
            text.contains(&format!("{} ms: DDI B: ", HOTPLUG_EVENT_LIMIT + 2)),
            "{text}"
        );
    }

    #[test]
    fn a_watch_that_has_seen_nothing_says_that_rather_than_nothing_at_all() {
        // Nothing in this test touches the statics: the file's hotplug section
        // is produced by `HotplugWatch`, and a watch that does not exist
        // produces no section at all.  What the test pins down is that "no
        // section" and "a section saying nothing happened" are different texts,
        // which is why the watch publishes itself before its first poll.
        let watch = HotplugWatch::new(bdf());
        let text = watch.render();
        assert!(
            text.contains("no transition since the state the sink step read at boot"),
            "{text}"
        );
        assert!(text.contains("every 250 ms"), "{text}");
        assert!(text.contains("The poll writes no register"), "{text}");
    }
}
