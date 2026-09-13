#!/usr/bin/env python3
"""Fail-closed hardware DUT gate for DUTs with or without a serial console.

The gate knows nothing about a particular lab controller.  A self-hosted
runner supplies local commands through its protected runner configuration, and
the gate drives one *observation channel* per cold boot:

``serial`` (default)
    ``THEKERNEL_DUT_SERIAL_CAPTURE_CMD`` captures the DUT's serial output and
    must return only after a *normal* guest shutdown.  The gate then enforces
    the full KTAP contract on the transcript and requires the hook to attest
    the shutdown.

``network``
    ``THEKERNEL_DUT_NETWORK_CAPTURE_CMD`` fetches the same transcript over a
    network-reachable acceptance channel and attests the same shutdown.  This
    is the serial channel with a different transport, so the *same* KTAP and
    shutdown validators apply; a DUT that starts life with no serial port
    graduates to full gating the moment it has an address.

``screen``
    ``THEKERNEL_DUT_SCREEN_CAPTURE_CMD`` records the screen, because that is
    the DUT's only output.  The gate validates the frames it wrote (real P6
    PPM, declared resolution, not blank) and the capture's verdict file, which
    names the frame where the completion banner is legible and a later frame
    proving the capture kept looking afterwards.  Screen evidence cannot prove
    per-test KTAP results; the gate says so out loud instead of pretending
    otherwise.  See the trust boundary below.

Trust boundary
--------------
A capture hook is trusted to *observe* the DUT, exactly as the serial hook
already was: it is protected runner configuration, not gate input, and the
gate has no way to re-run the boot.  What the gate can do is refuse anything
the hook did not actually produce.  Every channel therefore validates the
evidence itself: frame files must exist with matching hashes of structure
(a real PPM header, a plausible resolution, non-blank pixels), indices must be
contiguous, the verdict must name this DUT and this run, and the shutdown
attestation must be present.  A channel that cannot supply its evidence fails
the run; a missing hook is an error, never a skip.

Environment supplied to every hook: ``THEKERNEL_DUT_ARTIFACT_DIR``,
``THEKERNEL_DUT_KERNEL``, ``THEKERNEL_DUT_ESP``, ``THEKERNEL_DUT_ROOTFS``,
``THEKERNEL_DUT_RUN``, ``THEKERNEL_DUT_OBSERVATION``.  Transcript channels also
get ``THEKERNEL_DUT_TRANSCRIPT`` (and the legacy ``THEKERNEL_DUT_SERIAL_LOG``
alias) plus ``THEKERNEL_DUT_SHUTDOWN_STATUS``; the screen channel gets
``THEKERNEL_DUT_SCREEN_DIR`` and ``THEKERNEL_DUT_SCREEN_VERDICT``.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from tools.ktap import KtapError, clean_shutdown_attested, validate_ktap_log

#: The product artifacts every hardware tier boots.  A DUT gate that cannot
#: name what it booted is not gating anything.
REQUIRED_ARTIFACTS = ("kernel-x86_64", "kernel-x86_64.esp", "rootfs-x86.img")

#: Marker the guest prints after the last test.  The screen channel cannot read
#: it; it exists so the capture's verdict and the transcript agree on one name.
COMPLETION_MARKER = "# THEKERNEL_SYSTEM_TEST_COMPLETE"

SCREEN_FRAME_NAME = re.compile(r"^frame-(\d{4})\.ppm$")
SCREEN_VERDICT_LIMIT = 4096
SCREEN_MIN_FRAMES = 2
#: A frame whose most common sampled colour covers more than this fraction of
#: the samples is treated as a blank screen.  Its job is to catch a black
#: frame, a sink's "no signal" card or a lone stray pixel, not to grade the
#: picture: console text filling a screen is far above this floor.
SCREEN_BLANK_FRACTION = 0.995
SCREEN_SAMPLES = 4096
SCREEN_REQUIRED_KEYS = (
    "version",
    "dut",
    "run",
    "resolution",
    "frames",
    "completion_frame",
    "last_frame",
    "checker",
)


class GateError(RuntimeError):
    """A missing DUT contract or failing hardware test."""


@dataclass(frozen=True)
class RunPaths:
    """Where one cold boot's evidence lands."""

    transcript: Path
    shutdown_status: Path
    screen_dir: Path
    screen_verdict: Path


@dataclass(frozen=True)
class Channel:
    """A first-class way of observing one DUT boot."""

    name: str
    capture_hook: str
    capture_description: str
    validate: Callable[[RunPaths, str, int], None]
    detail: str
    #: Directory the screen channel writes into, created before the hook runs.
    needs_screen_dir: bool = False


@dataclass(frozen=True)
class Profile:
    """What a named DUT must prove, independent of how it is observed."""

    name: str
    observation: str
    runs: int
    #: Exact cold-boot count a certification run must request, if fixed.
    required_runs: int | None = None
    required_runs_message: str = ""

    def check_runs(self, runs: int) -> None:
        if self.required_runs is not None and runs != self.required_runs:
            raise GateError(self.required_runs_message)


def absolute_non_tmpfs(path: Path, *, name: str) -> Path:
    resolved = path.expanduser().resolve()
    if not resolved.is_absolute():
        raise GateError(f"{name} must be an absolute path")
    if resolved == Path("/tmp") or Path("/tmp") in resolved.parents:
        raise GateError(f"{name} must not be under /tmp")
    if resolved == Path("/dev/shm") or Path("/dev/shm") in resolved.parents:
        raise GateError(f"{name} must not be under /dev/shm")
    return resolved


def validate_artifacts(artifact_dir: Path) -> dict[str, Path]:
    artifact_dir = artifact_dir.resolve()
    if not artifact_dir.is_dir():
        raise GateError(f"product artifact directory does not exist: {artifact_dir}")
    artifacts: dict[str, Path] = {}
    for filename in REQUIRED_ARTIFACTS:
        path = (artifact_dir / filename).resolve()
        if path.parent != artifact_dir or not path.is_file() or path.is_symlink():
            raise GateError(f"required product artifact is missing or unsafe: {filename}")
        if path.stat().st_size == 0:
            raise GateError(f"required product artifact is empty: {filename}")
        artifacts[filename] = path
    return artifacts


def required_command(name: str) -> str:
    command = os.environ.get(name, "").strip()
    if not command:
        raise GateError(f"required protected runner hook is unset: {name}")
    return command


def read_text(path: Path, *, description: str) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        raise GateError(f"cannot read {description} {path}: {error}") from error


def validate_ktap(log_path: Path) -> None:
    text = read_text(log_path, description="DUT transcript")
    try:
        validate_ktap_log(text)
    except KtapError as error:
        raise GateError(str(error)) from error


def validate_clean_shutdown(status_path: Path) -> None:
    try:
        status = status_path.read_text(encoding="utf-8")
    except OSError as error:
        raise GateError(f"DUT capture hook did not write shutdown status: {error}") from error
    if not clean_shutdown_attested(status):
        raise GateError("DUT did not report a normal guest shutdown")


def validate_transcript_evidence(paths: RunPaths, dut: str, run: int) -> None:
    """The full contract: a KTAP transcript plus an attested clean shutdown."""

    del dut, run  # The transcript carries no DUT identity to cross-check.
    validate_ktap(paths.transcript)
    validate_clean_shutdown(paths.shutdown_status)


@dataclass(frozen=True)
class PpmHeader:
    width: int
    height: int
    maxval: int
    data_offset: int


def parse_ppm_header(path: Path) -> PpmHeader:
    """Parse a binary P6 PPM header, rejecting anything malformed or truncated."""

    try:
        size = path.stat().st_size
        with path.open("rb") as handle:
            head = handle.read(1024)
    except OSError as error:
        raise GateError(f"cannot read screen frame {path}: {error}") from error
    if len(head) < 2 or head[:2] != b"P6":
        raise GateError(f"screen frame is not a binary P6 PPM: {path.name}")
    fields: list[int] = []
    index = 2
    while len(fields) < 3:
        while index < len(head) and head[index : index + 1].isspace():
            index += 1
        if index < len(head) and head[index : index + 1] == b"#":
            while index < len(head) and head[index : index + 1] not in (b"\n", b"\r"):
                index += 1
            continue
        start = index
        while index < len(head) and head[index : index + 1].isdigit():
            index += 1
        if start == index:
            raise GateError(f"screen frame has a malformed PPM header: {path.name}")
        fields.append(int(head[start:index]))
        if len(fields) == 3:
            index += 1  # Exactly one whitespace byte separates header from data.
    width, height, maxval = fields
    if width <= 0 or height <= 0:
        raise GateError(f"screen frame has a non-positive size: {path.name}")
    if not 1 <= maxval <= 255:
        # 16-bit samples are legal PPM but no capture tool here produces them.
        raise GateError(f"screen frame has an unsupported PPM maxval {maxval}: {path.name}")
    expected = index + width * height * 3
    if size < expected:
        raise GateError(
            f"screen frame is truncated: {path.name} holds {size} bytes, "
            f"a {width}x{height} frame needs {expected}"
        )
    return PpmHeader(width=width, height=height, maxval=maxval, data_offset=index)


def frame_is_blank(path: Path, header: PpmHeader) -> bool:
    """Sample a frame and report whether the screen showed one flat colour."""

    pixels = header.width * header.height
    step = max(1, pixels // SCREEN_SAMPLES)
    counts: dict[bytes, int] = {}
    total = 0
    try:
        with path.open("rb") as handle:
            for number in range(0, pixels, step):
                handle.seek(header.data_offset + number * 3)
                sample = handle.read(3)
                if len(sample) != 3:
                    raise GateError(f"screen frame ends early: {path.name}")
                counts[sample] = counts.get(sample, 0) + 1
                total += 1
    except OSError as error:
        raise GateError(f"cannot read screen frame {path}: {error}") from error
    if total == 0:
        raise GateError(f"screen frame has no pixels to sample: {path.name}")
    return max(counts.values()) / total > SCREEN_BLANK_FRACTION


def parse_screen_verdict(verdict_path: Path) -> dict[str, str]:
    try:
        size = verdict_path.stat().st_size
    except OSError as error:
        raise GateError(f"screen capture hook wrote no verdict: {error}") from error
    if size > SCREEN_VERDICT_LIMIT:
        raise GateError(f"screen verdict is implausibly large: {verdict_path}")
    values: dict[str, str] = {}
    for number, raw in enumerate(read_text(verdict_path, description="screen verdict").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        key, value = key.strip(), value.strip()
        if not separator or not key:
            raise GateError(f"screen verdict line {number} is not key=value: {raw!r}")
        if key in values:
            raise GateError(f"screen verdict repeats key {key}")
        values[key] = value
    missing = [key for key in SCREEN_REQUIRED_KEYS if key not in values]
    if missing:
        raise GateError(f"screen verdict is missing required key(s): {', '.join(missing)}")
    return values


def screen_frames(screen_dir: Path) -> dict[int, Path]:
    if not screen_dir.is_dir():
        raise GateError(f"screen capture hook wrote no frame directory: {screen_dir}")
    frames: dict[int, Path] = {}
    for entry in sorted(screen_dir.iterdir()):
        match = SCREEN_FRAME_NAME.match(entry.name)
        if not match:
            continue
        if not entry.is_file() or entry.is_symlink():
            raise GateError(f"screen frame is not a regular file: {entry.name}")
        frames[int(match.group(1))] = entry
    if not frames:
        raise GateError(f"screen capture hook wrote no frames to {screen_dir}")
    if sorted(frames) != list(range(len(frames))):
        raise GateError("screen frame indices are not contiguous from frame-0000.ppm")
    return frames


def validate_screen_evidence(paths: RunPaths, dut: str, run: int) -> None:
    """Validate screen-only evidence: liveness plus an attested completion banner.

    The frame *contents* beyond "the screen was not blank" are attested by the
    capture hook's checker, which is named in the verdict.  This channel cannot
    establish the KTAP contract; ``--observation network`` does that.
    """

    # Frames first: "the hook recorded nothing" is the failure a human needs to
    # see, not a downstream complaint about the verdict that describes them.
    frames = screen_frames(paths.screen_dir)
    verdict = parse_screen_verdict(paths.screen_verdict)
    if verdict["version"] != "1":
        raise GateError(f"screen verdict version is not 1: {verdict['version']!r}")
    if verdict["dut"] != dut:
        raise GateError(f"screen verdict names DUT {verdict['dut']!r}, expected {dut!r}")
    try:
        declared_run = int(verdict["run"])
    except ValueError as error:
        raise GateError(f"screen verdict run is not an integer: {verdict['run']!r}") from error
    if declared_run != run:
        raise GateError(f"screen verdict is for run {declared_run}, expected {run}")
    if not verdict["checker"] or len(verdict["checker"]) > 200:
        raise GateError("screen verdict must name the checker that read the banner")
    if any(character in verdict["checker"] for character in "\r\n\t"):
        raise GateError("screen verdict checker name contains control characters")
    resolution = verdict["resolution"]
    match = re.fullmatch(r"([1-9][0-9]*)x([1-9][0-9]*)", resolution)
    if not match:
        raise GateError(f"screen verdict resolution is not WxH: {resolution!r}")

    frames = screen_frames(paths.screen_dir)
    try:
        declared_frames = int(verdict["frames"])
    except ValueError as error:
        raise GateError(f"screen verdict frames is not an integer: {verdict['frames']!r}") from error
    if declared_frames < SCREEN_MIN_FRAMES:
        raise GateError(f"screen evidence needs at least {SCREEN_MIN_FRAMES} frames")
    if declared_frames != len(frames):
        raise GateError(
            f"screen verdict declares {declared_frames} frames but {len(frames)} are present"
        )
    headers = {index: parse_ppm_header(path) for index, path in frames.items()}
    wanted = (int(match.group(1)), int(match.group(2)))
    for key in ("completion_frame", "last_frame"):
        if Path(verdict[key]).name != verdict[key]:
            raise GateError(f"screen verdict {key} must be a bare filename: {verdict[key]!r}")
        if SCREEN_FRAME_NAME.match(verdict[key]) is None:
            raise GateError(f"screen verdict {key} is not a frame filename: {verdict[key]!r}")
        index = int(SCREEN_FRAME_NAME.match(verdict[key]).group(1))
        if index not in frames:
            raise GateError(f"screen verdict names a missing frame: {verdict[key]}")
        header = headers[index]
        if (header.width, header.height) != wanted:
            raise GateError(
                f"screen frame {verdict[key]} is {header.width}x{header.height}, "
                f"verdict declares {resolution}"
            )
        # Only the frame that carries the evidence has to show something.  The
        # last frame may legitimately be a dark screen: a DUT that powers itself
        # off after the banner is a success, and a uniform frame would then be
        # the honest record of it.  Its job is to prove the capture was still
        # looking after the banner appeared, which its index already does.
        if key == "completion_frame" and frame_is_blank(frames[index], header):
            raise GateError(f"screen frame {verdict[key]} is blank; it cannot show a banner")
    completion = int(SCREEN_FRAME_NAME.match(verdict["completion_frame"]).group(1))
    last = int(SCREEN_FRAME_NAME.match(verdict["last_frame"]).group(1))
    if last <= completion:
        raise GateError(
            "screen evidence must keep observing after the completion banner: "
            f"last_frame {verdict['last_frame']} does not follow {verdict['completion_frame']}"
        )


CHANNELS: dict[str, Channel] = {
    "serial": Channel(
        name="serial",
        capture_hook="THEKERNEL_DUT_SERIAL_CAPTURE_CMD",
        capture_description="serial capture",
        validate=validate_transcript_evidence,
        detail="KTAP transcript and attested clean shutdown from the serial console",
    ),
    "network": Channel(
        name="network",
        capture_hook="THEKERNEL_DUT_NETWORK_CAPTURE_CMD",
        capture_description="network acceptance capture",
        validate=validate_transcript_evidence,
        detail="KTAP transcript and attested clean shutdown over the network channel",
    ),
    "screen": Channel(
        name="screen",
        capture_hook="THEKERNEL_DUT_SCREEN_CAPTURE_CMD",
        capture_description="screen capture",
        validate=validate_screen_evidence,
        detail="screen frames plus an attested on-screen completion banner (no KTAP contract)",
        needs_screen_dir=True,
    ),
}

PROFILES: dict[str, Profile] = {
    "panther-lake": Profile(
        name="panther-lake",
        observation="serial",
        runs=3,
        required_runs=3,
        required_runs_message="Panther Lake certification requires exactly three cold boots",
    ),
    "n305": Profile(
        name="n305",
        observation="screen",
        runs=3,
    ),
}


def run_hook(command: str, *, environment: dict[str, str], description: str) -> None:
    completed = subprocess.run(
        command,
        shell=True,
        executable="/bin/bash",
        env=environment,
        check=False,
    )
    if completed.returncode:
        raise GateError(f"{description} hook failed with exit status {completed.returncode}")


@dataclass
class RunEvidence:
    """What the gate established for one cold boot; printed so a human can audit it."""

    run: int
    channel: str
    detail: str
    facts: dict[str, str] = field(default_factory=dict)


def observe(
    channel: Channel, environment: dict[str, str], paths: RunPaths, *, dut: str, run: int
) -> RunEvidence:
    command = required_command(channel.capture_hook)
    if channel.needs_screen_dir:
        paths.screen_dir.mkdir(parents=True, exist_ok=True)
    run_hook(command, environment=environment, description=f"cold boot {run} {channel.capture_description}")
    channel.validate(paths, dut, run)
    facts: dict[str, str] = {}
    if channel.name == "screen":
        verdict = parse_screen_verdict(paths.screen_verdict)
        facts = {
            "frames": verdict["frames"],
            "resolution": verdict["resolution"],
            "completion_frame": verdict["completion_frame"],
            "checker": verdict["checker"],
        }
    return RunEvidence(run=run, channel=channel.name, detail=channel.detail, facts=facts)


def run_gate(
    artifact_dir: Path,
    state_dir: Path,
    *,
    dut: str = "panther-lake",
    observation: str | None = None,
    runs: int | None = None,
) -> list[RunEvidence]:
    profile = PROFILES.get(dut)
    if profile is None:
        raise GateError(f"unknown DUT profile {dut!r}; known: {', '.join(sorted(PROFILES))}")
    channel_name = observation or os.environ.get("THEKERNEL_DUT_OBSERVATION") or profile.observation
    channel = CHANNELS.get(channel_name)
    if channel is None:
        raise GateError(
            f"unknown observation channel {channel_name!r}; known: {', '.join(sorted(CHANNELS))}"
        )
    total = profile.runs if runs is None else runs
    if total < 1:
        raise GateError("a gate run needs at least one cold boot")
    profile.check_runs(total)
    artifacts = validate_artifacts(artifact_dir)
    state_dir.mkdir(parents=True, exist_ok=True)
    if not state_dir.is_dir():
        raise GateError(f"cannot create DUT state directory: {state_dir}")
    power_cycle = required_command("THEKERNEL_DUT_POWER_CYCLE_CMD")
    boot_once = required_command("THEKERNEL_DUT_BOOT_ONCE_CMD")

    evidence: list[RunEvidence] = []
    for number in range(1, total + 1):
        transcript = state_dir / f"cold-boot-{number}.serial.log"
        shutdown_status = state_dir / f"cold-boot-{number}.shutdown"
        screen_dir = state_dir / f"cold-boot-{number}.screen"
        screen_verdict = screen_dir / "verdict.txt"
        paths = RunPaths(
            transcript=transcript,
            shutdown_status=shutdown_status,
            screen_dir=screen_dir,
            screen_verdict=screen_verdict,
        )
        for path in (transcript, shutdown_status):
            path.unlink(missing_ok=True)
        environment = {
            **os.environ,
            "THEKERNEL_DUT_ARTIFACT_DIR": str(artifact_dir),
            "THEKERNEL_DUT_KERNEL": str(artifacts["kernel-x86_64"]),
            "THEKERNEL_DUT_ESP": str(artifacts["kernel-x86_64.esp"]),
            "THEKERNEL_DUT_ROOTFS": str(artifacts["rootfs-x86.img"]),
            "THEKERNEL_DUT_RUN": str(number),
            "THEKERNEL_DUT_OBSERVATION": channel.name,
            # Transport-neutral name; the serial alias stays for older runners.
            "THEKERNEL_DUT_TRANSCRIPT": str(transcript),
            "THEKERNEL_DUT_SERIAL_LOG": str(transcript),
            "THEKERNEL_DUT_SHUTDOWN_STATUS": str(shutdown_status),
            "THEKERNEL_DUT_SCREEN_DIR": str(screen_dir),
            "THEKERNEL_DUT_SCREEN_VERDICT": str(screen_verdict),
        }
        run_hook(power_cycle, environment=environment, description=f"cold boot {number} power-cycle")
        run_hook(boot_once, environment=environment, description=f"cold boot {number} one-shot boot")
        evidence.append(observe(channel, environment, paths, dut=dut, run=number))
    return evidence


def report(dut: str, channel: Channel, evidence: list[RunEvidence]) -> None:
    for item in evidence:
        facts = " ".join(f"{key}={value}" for key, value in sorted(item.facts.items()))
        suffix = f" {facts}" if facts else ""
        print(
            f"dut-gate: {dut}: cold boot {item.run}/{len(evidence)}: PASS "
            f"observation={item.channel} evidence={item.detail}{suffix}",
            flush=True,
        )
    print(
        f"dut-gate: {dut}: PASS {len(evidence)} cold boot(s), observation={channel.name}",
        flush=True,
    )
    if channel.name == "screen":
        print(
            "dut-gate: screen evidence proves liveness and an on-screen completion banner; "
            "it does not verify the KTAP contract. Use --observation network once the DUT "
            "is network-reachable.",
            flush=True,
        )


def build_parser(default_dut: str) -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact-dir", required=True, type=Path)
    parser.add_argument("--state-dir", required=True, type=Path)
    parser.add_argument("--dut", default=default_dut, choices=sorted(PROFILES))
    parser.add_argument("--observation", choices=sorted(CHANNELS))
    parser.add_argument("--runs", type=int)
    return parser


def main(argv: list[str] | None = None, *, default_dut: str = "panther-lake") -> int:
    args = build_parser(default_dut).parse_args(argv)
    try:
        artifact_dir = absolute_non_tmpfs(args.artifact_dir, name="--artifact-dir")
        state_dir = absolute_non_tmpfs(args.state_dir, name="--state-dir")
        evidence = run_gate(
            artifact_dir,
            state_dir,
            dut=args.dut,
            observation=args.observation,
            runs=args.runs,
        )
    except GateError as error:
        print(f"dut-gate: {getattr(args, 'dut', default_dut)}: FAIL {error}", file=sys.stderr)
        return 1
    profile = PROFILES[args.dut]
    channel = CHANNELS[args.observation or os.environ.get("THEKERNEL_DUT_OBSERVATION") or profile.observation]
    report(args.dut, channel, evidence)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
