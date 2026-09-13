//! Phase 2.1 and the connector: power the DDC pin pair, then ask what is
//! behind it.
//!
//! Reference §11 phase 2 is three steps, and their order is the point:
//!
//! 1. **2.1** Enable the AUX/DDC power well for the port --
//!    `ICL_PWR_WELL_CTL_AUX2 (0x45444)`, index 0 for `AUX_A` -- "needed for
//!    GMBUS/DCC on that pin pair".  The reference names what skipping it looks
//!    like: *"GMBUS returns NAK on every address, always."*
//! 2. **2.2** Enable hotplug, then immediately read the status once.
//! 3. **2.3** Read the EDID over GMBUS, trying pin index 1 (DDI A) and then
//!    pin index 2 (DDI B), because "a monitor's EDID will appear on exactly one
//!    of them and that identifies your physical port".
//!
//! [`sink`] is steps 2.2 and 2.3, and says so: it reads the bus and *names*
//! [`GmbusError::AuxWellDown`] rather than enabling a well behind the power
//! workstream's back.  This module is the composition and nothing more -- it
//! requests the well pair the candidate pins sit behind, hands the same bus to
//! [`sink::probe_one`], and turns the answer into the single value a modeset
//! consumes: which pin, which DDI, which EDID, which timing, and what the
//! hotplug pin says.
//!
//! The well step is why this module exists at all.  GMBUS is a DDC channel
//! behind a power gate, so a monitor can be plugged in, wired to a pin this
//! driver selects correctly, and still NAK every address because the gate in
//! front of the channel is shut -- and on a machine whose only console is the
//! screen this driver is trying to light, a step that is missing from the
//! sequence is a step whose symptom has no other explanation in the log.
//!
//! ## A well that does not come up is recorded, not fatal
//!
//! The firmware may already hold a well on under a request bit this driver does
//! not own, and which of `AUX_C` and above an ADL-N SKU physically has is
//! reference §4.2's `[GAP]`.  A `STATE` bit that never sets is therefore a
//! finding: it goes into [`Connector::wells`] and into the report, and the
//! probe runs anyway.  On a machine where that *is* the whole problem, the
//! reference's own symptom is a bus that NAKs every address, which the GMBUS
//! layer already names -- so the log ends up carrying both the well that is off
//! and the pins that NAKed, which is the pair of facts a person needs.
//!
//! Nothing here has run on the target hardware.

use alloc::{format, string::String, vec::Vec};

use axlog::{info, warn};

use super::{
    gmbus::{self, AuxWell, EdidBytes, GmbusError, MonotonicTimer, Pin, PollTimer, SinkProbe},
    hpd::{Ddi, HpdError, HpdStatus},
    pci::Bdf,
    power::{self, PowerError, Requesters, Well, WellObservation},
    probe::{ProbeReport, WindowStatus},
    regs::Registers,
    sink,
};
use crate::drm::modes::ModePlan;

/// The pins §11 phase 2.3 says to try, each with the AUX/DDC power well its DDC
/// channel sits behind.
///
/// Pin index 1 is DDI A and pin index 2 is DDI B, one-based; the well each one
/// needs is `ICL_PWR_WELL_CTL_AUX2` index 0 and index 1 (reference §4.2, and
/// `[I915]` `i915_reg.h:3674-3698`, whose `XE_LPD` column puts `AUX_A` at 0 and
/// `AUX_B` at 1).  The pairing is checked against [`Pin::aux_well`] -- the GMBUS
/// layer's own spelling of the same mapping -- and against the register table,
/// by a test, so the two cannot drift apart.
///
/// [`Pin::DDC`] also asks DDI C, and this list deliberately does not power it:
/// §11 phase 2.3 names pins 1 and 2 as the candidates on this machine, and
/// which of `AUX_C` and above an ADL-N SKU has is §4.2's `[GAP]`.  A monitor
/// that were on DDI C is still asked, and is reported by the GMBUS layer as pin
/// 3 with `AUX_C` reading back off -- a fact about that pin rather than a
/// silent failure here.
const CANDIDATES: [(Pin, Well); 2] = [(Pin::DdiA, power::AUX_A), (Pin::DdiB, power::AUX_B)];

/// The one value the modeset takes: a pin a monitor answered on, and everything
/// the bring-up learned about it.
///
/// Every field is a fact that was read, not a conclusion: the pin is the one an
/// EDID came back from, the DDI is [`Pin::ddi`]'s answer for it, the bytes are
/// the blocks that passed their own checks, the plan is `drm::modes`' decision
/// over those bytes, and the hotplug status is the live state read for that
/// DDI.  A caller that programs a mode programs *this*, and nothing else in the
/// driver has to agree with anything by convention.
#[derive(Clone, Debug)]
pub(crate) struct Connector {
    pub(crate) bdf: Bdf,
    /// The pin a monitor answered on.  §11 phase 2.3: this is what identifies
    /// the physical port.
    pub(crate) pin: Pin,
    /// The DDI that pin carries DDC for, from [`Pin::ddi`].
    pub(crate) ddi: Ddi,
    /// The validated EDID base block.
    pub(crate) edid: EdidBytes,
    /// The first extension block, when the sink declared one and it read.
    pub(crate) extension: Option<EdidBytes>,
    /// `drm::modes::plan_modeset`'s result over the blocks above.
    pub(crate) plan: ModePlan,
    /// The live hotplug state for this connector's DDI.
    pub(crate) hotplug: HpdStatus,
    /// What phase 2.1 found for each candidate well, for the log.
    pub(crate) wells: Vec<(AuxWell, WellObservation)>,
}

impl Connector {
    /// One line naming the port and the timing, for the boot log.
    ///
    /// The pin's `Display` already carries its DDI; the DDI is repeated because
    /// that is the number §11 phases 3 through 5 program the hardware with, and
    /// a reader comparing this line against those steps should not have to
    /// translate.
    pub(crate) fn describe(&self) -> String {
        format!(
            "display {}: {} carries DDI {}, {}; mode layer chose {} ({:?}{})",
            self.bdf,
            self.pin,
            self.ddi.name(),
            self.edid.describe(),
            self.plan.selection.mode,
            self.plan.selection.reason,
            if self.plan.strict {
                ""
            } else {
                ", lenient parse"
            },
        )
    }
}

/// Every way the composition above can fail to produce a [`Connector`].
///
/// One variant per condition, because the next move differs for each: a NAK is
/// a question about the cable, the port or the address; a block that did not
/// validate wants a slower read or a better cable; a well that never came up is
/// a message for the power workstream.  A single `Err(())` would collapse those
/// into "no display", which is the answer that wastes a day on hardware -- the
/// same argument [`GmbusError`] makes for itself.
#[derive(Clone, Debug)]
pub(crate) enum ConnectError {
    /// No pin answered at all, with what each pin said instead.
    NoMonitor { pins: SinkProbe },
    /// A sink answered and the block it returned did not validate twice, at two
    /// rates.  This is kept apart from [`ConnectError::NoMonitor`] because the
    /// two look identical in a log that says "no EDID": one is a missing
    /// display, the other is a display whose bytes cannot be trusted.
    EdidRejected { pin: Pin, error: GmbusError },
    /// The pin answered, but it carries no DDI, so there is no port to program.
    NoDdiForPin { pin: Pin },
    /// Hotplug for the connector's DDI could not be read.
    HotplugUnreadable { ddi: Ddi, error: HpdError },
    /// The sink step read hotplug for every DDI in [`Ddi::ALL`] and this one is
    /// not in its ledger.  A bug in this kernel, not a state of the machine.
    HotplugNotRecorded { ddi: Ddi },
    /// An AUX/DDC power well did not come up, and no monitor answered either.
    ///
    /// Recorded and reported rather than fatal by itself: §11 phase 2.1's
    /// symptom for a well that is off is a bus that NAKs every address, which
    /// the GMBUS layer names for itself, and the firmware may hold the well in
    /// a state this driver cannot turn on.  The observation is the handshake's
    /// record of the failure and `cause` is `power::enable_well`'s own account
    /// of it.
    WellDown {
        well: AuxWell,
        observation: WellObservation,
        cause: PowerError,
    },
    /// The sink step answered with a pin but no EDID, or with an EDID but no
    /// mode plan.  [`sink::probe_one`] builds those three from one answer, so
    /// reaching this means its ledger changed shape: a bug in this kernel,
    /// refused rather than unwrapped.
    SinkIncomplete,
}

impl ConnectError {
    /// One line for a log, complete enough to act on without the code open.
    ///
    /// The target machine's only console is the screen this driver is trying to
    /// light, so a message that says "connect failed" is a message nobody can
    /// act on.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::NoMonitor { pins } => format!(
                "no monitor answered on any DDC pin this kernel can select: {}.  Reference \
                 section 11 phase 2.3: the pin that answers is what identifies the physical port, \
                 so nothing here identifies one; each pin's own answer is in the list",
                summarise(pins)
            ),
            Self::EdidRejected { pin, error } => format!(
                "a sink answered on {pin} but the block it returned did not validate: {}.  \
                 Reference section 11 phase 2.3: do not proceed past this step until the EDID is \
                 valid, because a wrong EDID gives wrong timings and the bug it causes looks like \
                 a modeset bug",
                error.describe()
            ),
            Self::NoDdiForPin { pin } => format!(
                "{pin} answered with a valid EDID but carries no DDI, so there is no port for a \
                 modeset to program.  Pin::ddi is the only pin-to-DDI mapping this driver has, \
                 and the Type-C pins reach their DDC channel through the DKL PHY, which reference \
                 section 8.8 defers"
            ),
            Self::HotplugUnreadable { ddi, error } => format!(
                "DDI {ddi}: the hotplug read that says whether the port is live failed: {}.  \
                 Reference section 11 phase 2.2 reads SDEISR once after enabling HPD, and a \
                 connector whose port state cannot be read is not a connector",
                error.describe()
            ),
            Self::HotplugNotRecorded { ddi } => format!(
                "DDI {ddi}: the sink step reads hotplug for every DDI in Ddi::ALL and this one is \
                 not in its ledger, which is a bug in this kernel rather than a state of the \
                 machine"
            ),
            Self::WellDown {
                well,
                observation,
                cause,
            } => format!(
                "power well {well} did not come up, and no monitor answered on any pin either, so \
                 the AUX/DDC channel behind it never had a chance: {}.  Reference section 11 \
                 phase 2.1: this is what 'GMBUS returns NAK on every address, always' looks like \
                 from the power side.  The handshake ended with: {}",
                cause.describe(),
                observation.describe()
            ),
            Self::SinkIncomplete => String::from(
                "the sink step answered with a pin but no validated EDID, or with an EDID but no \
                 mode plan, which is a bug in this kernel: sink::probe_one builds those three \
                 from one answer",
            ),
        }
    }
}

impl core::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// What phase 2.1 and the sink step found, for the log and the debug file.
#[derive(Default)]
pub(crate) struct ConnectReport {
    /// One per display device a monitor answered on.
    pub(crate) connectors: Vec<Connector>,
    /// One per display device that did not produce a connector, with the
    /// reason.
    pub(crate) failures: Vec<(Bdf, ConnectError)>,
    /// Every device the sink step read hotplug for, and what each DDI answered.
    ///
    /// Kept because the reading is a step's result and not only a line in a
    /// log: the after-boot hotplug watch compares each poll against the state
    /// this pass already reported, and a watch that treated its own first look
    /// as an event would announce a monitor that was there all along.  It is
    /// kept here rather than in a report type of its own because this pass is
    /// the only place phase 2 runs -- reading the bus twice to fill two
    /// structures would be two transactions to answer one question.
    pub(crate) hotplug: Vec<(Bdf, Vec<HpdStatus>)>,
}

impl ConnectReport {
    /// The same text the log carries, for `/sys/kernel/debug/dri/0/intel_gpu`.
    ///
    /// Every line is prefixed, so a reader who pipes the file into a log can
    /// grep one word to find all of it, exactly as the probe's own report does.
    /// The well lines are here and not only in the log for the reason the
    /// module header gives: on a machine where GMBUS NAKs every address, the
    /// well that reads back off is the finding, and a boot log scrolls away.
    pub(crate) fn render(&self) -> String {
        if self.connectors.is_empty() && self.failures.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n--- the connector (reference section 11 phase 2) ---\n");
        for connector in &self.connectors {
            out.push_str(&format!("{}\n", connector.describe()));
            for (well, observation) in &connector.wells {
                out.push_str(&format!("  {well}: {}\n", observation.describe()));
            }
            out.push_str(&format!("  {}\n", connector.hotplug.describe()));
            if let Some(extension) = &connector.extension {
                out.push_str(&format!("  extension block: {}\n", extension.describe()));
            }
        }
        for (bdf, error) in &self.failures {
            out.push_str(&format!("display {bdf}: {}\n", error.describe()));
        }
        out
    }
}

/// Resolve one display device into the connector a modeset can program.
///
/// This is the seam the tests drive, exactly as [`sink::probe_one`] is: the
/// boot path passes the mapped aperture and the machine's clock, and a host
/// test passes a model of the controller and a clock it owns, so the whole
/// phase -- the wells, the bus, the EDID checks, the mode layer, hotplug --
/// runs on a machine with no display hardware.
///
/// The order is the reference's and it is load-bearing: the wells are requested
/// before the first GMBUS transaction, because a channel behind a shut gate
/// NAKs every address and looks exactly like a port with nothing on it.
pub(crate) fn resolve<R: Registers, T: PollTimer>(
    bdf: Bdf,
    regs: &R,
    timer: &T,
) -> Result<Connector, ConnectError> {
    resolve_device(bdf, regs, timer).outcome
}

/// One device's phase-2 outcome: what the sink step read, and what the
/// connector step made of it.
///
/// The two travel together because that reading is a result and not only a log
/// line: [`ConnectReport::hotplug`] keeps it as the baseline the after-boot
/// watch compares against, and this pass is the only place phase 2 runs.
pub(crate) struct Resolved {
    /// Every DDI the sink step read hotplug for, in [`Ddi::ALL`] order.
    pub(crate) hotplug: Vec<HpdStatus>,
    /// The connector, or the named reason there is none.
    pub(crate) outcome: Result<Connector, ConnectError>,
}

/// Resolve one display device, keeping the reading the sink step made.
///
/// [`resolve`] is this function without the reading.  The decisions live in a
/// closure so that every early return below still reaches it: a device that
/// produced no connector has still answered the hotplug read, and that answer
/// is the baseline the after-boot watch needs.
pub(crate) fn resolve_device<R: Registers, T: PollTimer>(
    bdf: Bdf,
    regs: &R,
    timer: &T,
) -> Resolved {
    let Wells { records, down } = enable_wells(regs);

    // Steps 2.2 and 2.3 are one call: hotplug enabled and read once, then the
    // EDID read over GMBUS with its header and checksum checks, then the mode
    // layer's plan over whatever validated.  Nothing about that read is
    // duplicated here.
    let device = sink::probe_one(bdf, regs, timer);
    let hotplug = device.hotplug.clone();
    let outcome = (move || -> Result<Connector, ConnectError> {
        let (Some(pin), Some(edid), Some(plan)) = (device.monitor, device.edid, device.plan) else {
            // A pin cannot answer without a block that validated -- GMBUS validates
            // header and checksum before it returns one -- so a read that produced
            // bytes this kernel would not believe is a different answer from a bus
            // that NAKed, and it is reported as itself.
            if let Some((pin, error)) = rejected(&device.pins) {
                return Err(ConnectError::EdidRejected { pin, error });
            }
            // Nothing answered anywhere.  A well that did not come up is the more
            // actionable of the two facts, and it carries the register words a
            // reader compares against §11 phase 1.3.
            if let Some((well, observation, cause)) = down {
                return Err(ConnectError::WellDown {
                    well,
                    observation,
                    cause,
                });
            }
            return Err(ConnectError::NoMonitor { pins: device.pins });
        };

        let Some(ddi) = pin.ddi() else {
            return Err(ConnectError::NoDdiForPin { pin });
        };
        // Hotplug was read for every DDI in the same pass; the connector takes the
        // line for its own DDI, or the named reason there is not one.
        let hotplug = match device.hotplug.iter().find(|status| status.ddi == ddi) {
            Some(status) => *status,
            None => {
                let error = device
                    .hotplug_errors
                    .iter()
                    .find(|(candidate, _)| *candidate == ddi)
                    .map(|(_, error)| *error);
                return Err(match error {
                    Some(error) => ConnectError::HotplugUnreadable { ddi, error },
                    None => ConnectError::HotplugNotRecorded { ddi },
                });
            }
        };

        Ok(Connector {
            bdf,
            pin,
            ddi,
            edid,
            extension: device.extension,
            plan,
            hotplug,
            wells: records,
        })
    })();
    Resolved { hotplug, outcome }
}

/// What phase 2.1 found on one display device.
struct Wells {
    /// Every candidate well, with what its handshake observed.  A well that
    /// never came up is in here too: §11 phase 2.1's whole diagnostic value is
    /// that the well which is off is named next to the pins that NAKed.
    records: Vec<(AuxWell, WellObservation)>,
    /// The first well that did not come up, in the order §11 phase 2.3 tries
    /// the pins, with its record and the reason.
    down: Option<(AuxWell, WellObservation, PowerError)>,
}

/// Request the AUX/DDC power well behind every candidate pin, before the bus is
/// touched.
///
/// §11 phase 2.1 is one register per pin pair: `ICL_PWR_WELL_CTL_AUX2
/// (0x45444)`, request bit `0x2 << (2 * index)`, state bit `0x1 << (2 * index)`
/// (reference §4.2; `[I915]` `i915_reg.h:3630-3631` for the bit arithmetic and
/// `:3674-3698` for the indices).  The request is left set on success -- the
/// well is needed for every later GMBUS and AUX transaction on that pair --
/// and `power::enable_well` withdraws the bit only when the state bit never
/// comes up, so a failure leaves the register as it was found.
fn enable_wells<R: Registers>(regs: &R) -> Wells {
    let mut records = Vec::with_capacity(CANDIDATES.len());
    let mut down = None;
    for (pin, well) in CANDIDATES {
        // The record is keyed by the GMBUS layer's `AuxWell`, because that is
        // the value its own error names when a pin NAKs: the log line from this
        // step and the log line from the bus have to be about the same object.
        let reported = pin.aux_well();
        match power::enable_well(regs, well) {
            Ok(observation) => {
                info!("intel-connect: {}", observation.describe());
                records.push((reported, observation));
            }
            Err(cause) => {
                // §11 phase 2.1, "looks like it did nothing": the state bit
                // never sets.  Recorded and reported here, not fatal: the
                // firmware may already hold the well on, and what the machine
                // does about it is the bus's answer on each pin, which follows.
                let observation = observation_of(reported, &cause);
                warn!("intel-connect: {}", cause.describe());
                records.push((reported, observation));
                if down.is_none() {
                    down = Some((reported, observation, cause));
                }
            }
        }
    }
    Wells { records, down }
}

/// The record a failed well enable leaves behind.
///
/// `power::enable_well` reports a failure as a [`PowerError`], because its
/// caller is expected to stop; this step does not stop, so the failure is
/// turned back into the shape a successful enable produces and recorded beside
/// it.  A `STATE` bit that never set is reported with the words the error kept
/// -- the register as it read at the moment of failure, and the requesters that
/// were holding it then -- which are the numbers §11 phase 1.3 asks a reader to
/// compare; both control words hold that one snapshot, because the value from
/// before the request was consumed by the handshake and the register has since
/// been rolled back, so the snapshot is the only honest number left.
///
/// The other two ways to fail -- a window that does not reach
/// `ICL_PWR_WELL_CTL_AUX2`, a write the register table refuses -- are bugs in
/// this kernel rather than states of the machine: there are no register words
/// to report, and the [`PowerError`] beside this record is their whole account.
fn observation_of(well: AuxWell, cause: &PowerError) -> WellObservation {
    match cause {
        PowerError::WellStateNeverSet {
            control,
            requesters,
            ..
        } => WellObservation {
            name: well.name(),
            index: well.index(),
            control_before: *control,
            control_after: *control,
            already_on: false,
            state_set: false,
            pg0: None,
            pg: None,
            requesters: *requesters,
        },
        _ => WellObservation {
            name: well.name(),
            index: well.index(),
            control_before: 0,
            control_after: 0,
            already_on: false,
            state_set: false,
            pg0: None,
            pg: None,
            requesters: Requesters {
                bios: false,
                driver: false,
                kvmr: false,
                debug: false,
            },
        },
    }
}

/// The first pin whose read produced a block that did not validate.
///
/// §11.1 separates the two answers a failed read can give: a NAK is a question
/// about the cable, the port or the address, while a block whose header or
/// checksum is wrong is a read that happened and produced bytes this kernel
/// will not believe.  By the time this is asked, the GMBUS layer has already
/// made its second attempt at the lower rate, so the bytes are as good as this
/// bus is going to give.
fn rejected(pins: &SinkProbe) -> Option<(Pin, GmbusError)> {
    pins.outcomes
        .iter()
        .find_map(|outcome| match outcome.result {
            Err(error) if did_not_validate(error) => Some((outcome.pin, error)),
            _ => None,
        })
}

/// Whether an error means "bytes came back and did not validate".
fn did_not_validate(error: GmbusError) -> bool {
    matches!(
        error,
        GmbusError::EdidHeader { .. } | GmbusError::EdidChecksum { .. }
    )
}

/// What each pin answered, one clause each, for a log line.
///
/// The pin's own `Display` carries the pin index, the channel name and the DDI,
/// so a reader of the one-line summary has the same identification the
/// multi-line report gives.
fn summarise(pins: &SinkProbe) -> String {
    let mut parts = Vec::with_capacity(pins.outcomes.len());
    for outcome in &pins.outcomes {
        parts.push(match &outcome.result {
            Ok(edid) => format!("{}: {}", outcome.pin, edid.describe()),
            Err(error) => format!("{}: {}", outcome.pin, error.describe()),
        });
    }
    parts.join("; ")
}

/// Ask every display device the probe mapped what is attached to it, and pick
/// the connector.
///
/// The devices are the ones whose register window the probe mapped and
/// identified; a device with no window is skipped rather than guessed at, which
/// is the same policy the probe applies to a part it has no model for.  One
/// device's failure never stops the next, and both outcomes are kept: a
/// connector for the modeset, and the named reason for every device that did
/// not produce one.
pub(crate) fn resolve_at_boot(report: &ProbeReport) -> ConnectReport {
    let mut result = ConnectReport::default();
    for found in &report.displays {
        let WindowStatus::Mapped { window, .. } = found.status else {
            continue;
        };
        info!(
            "intel-connect: powering the DDC pin pair on {} and asking what is attached \
             (reference section 11 phase 2)",
            found.info.bdf
        );
        let resolved = resolve_device(found.info.bdf, &window, &MonotonicTimer);
        // The reading is kept whatever the outcome: a device that produced no
        // connector has still answered the hotplug read, and that answer is
        // what the after-boot watch compares its first poll against.
        result.hotplug.push((found.info.bdf, resolved.hotplug));
        match resolved.outcome {
            Ok(connector) => {
                info!("intel-connect: {}", connector.describe());
                result.connectors.push(connector);
            }
            Err(error) => {
                warn!(
                    "intel-connect: display {}: {}",
                    found.info.bdf,
                    error.describe()
                );
                result.failures.push((found.info.bdf, error));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests;
