"""Repository-owned verification tiers; CI only provisions and selects a tier."""
from __future__ import annotations

import os
import ctypes
from contextlib import contextmanager
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tomllib
import time
from dataclasses import dataclass

from tools.product_state import REPO_ROOT, ProductError, state_root


@dataclass(frozen=True)
class Stage:
    name: str
    failure: str
    command: tuple[str, ...]
    timeout: int


def plan(tier: str, state: Path) -> list[Stage]:
    cli = (sys.executable, str(REPO_ROOT / "tools/thekernel.py"))
    if tier == "hardware":
        return [Stage("cpu-kvm", "test", (*cli, "test", "--suite", "cpu", "--smp", "4", "--accel", "kvm"), 1800)]
    stages = [
        Stage("dependency-layers", "static", (sys.executable, "scripts/ci/check_cargo_dependency_layers.py"), 120),
        Stage("graphics-config-seatd", "static", ("scripts/build-graphics-rootfs.sh", "--flavor", "q35-graphics-seatd", "--check"), 120),
        Stage("graphics-config-desktop", "static", ("scripts/build-graphics-rootfs.sh", "--flavor", "q35-software-desktop", "--check"), 120),
        Stage("host", "test", (*cli, "test", "--suite", "host"), 1800),
        Stage("build", "build", (*cli, "build", "--smp", "4", "--memory", "512M"), 1800),
        Stage("lint", "lint", (*cli, "lint", "--smp", "4", "--memory", "512M"), 1800),
        Stage("guest-tcg", "test", (*cli, "test", "--suite", "guest", "--smp", "4", "--memory", "512M", "--accel", "tcg", "--no-build", "--timeout", "300"), 360),
    ]
    if tier == "full":
        graphics = state / "verify-graphics"
        # Source the maintained build pin rather than duplicating its version.
        pins = dict(line.split("=", 1) for line in (REPO_ROOT / "config/graphics/pins.env").read_text().splitlines()
                    if line and not line.startswith("#") and "=" in line)
        output = graphics / "seatd"
        stages += [
            Stage("graphics-rootfs", "build", ("scripts/build-graphics-rootfs.sh", "--flavor", "q35-graphics-seatd", "--fetch-buildroot", "--buildroot-dir", str(graphics / ("buildroot-" + pins["BUILDROOT_VERSION"])), "--output", str(output), "--download-dir", str(graphics / "downloads")), 10800),
            Stage("pixman", "test", (*cli, "test", "--suite", "graphics", "--smp", "4", "--accel", "tcg", "--timeout", "300", "--rootfs", str(output / "images/rootfs.ext2"), "--flavor", "q35-graphics-seatd", "--graphics-profile", "headless", "--screenshot", str(graphics / "seatd.ppm"), "--workdir", str(graphics / "run")), 900),
        ]
    return stages


def _children() -> set[int]:
    return {int(pid) for pid in Path(f"/proc/self/task/{os.getpid()}/children").read_text().split()}


@contextmanager
def _stage_descendants():
    # QEMU deliberately creates its own session. Linux subreaper adoption
    # keeps those descendants ours even when the stage leader is SIGKILLed.
    libc = ctypes.CDLL(None, use_errno=True)
    previous = ctypes.c_int()
    if libc.prctl(37, ctypes.byref(previous), 0, 0, 0) or libc.prctl(36, 1, 0, 0, 0):
        raise ProductError("verify: supervision: FAIL type=environment cannot enable child subreaper")
    baseline = _children()
    try:
        yield
    finally:
        deadline = time.monotonic() + 5
        try:
            while children := _children() - baseline:
                for pid in children:
                    # A pidfd binds the signal to this child, never a recycled PID.
                    try:
                        descriptor = os.pidfd_open(pid)
                    except ProcessLookupError:
                        continue
                    try:
                        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    finally:
                        os.close(descriptor)
                    try:
                        os.waitpid(pid, os.WNOHANG)
                    except ChildProcessError:
                        pass
                if time.monotonic() >= deadline:
                    raise ProductError("verify: supervision: FAIL type=environment descendants did not exit")
                time.sleep(0.01)
        finally:
            libc.prctl(36, previous.value, 0, 0, 0)


def execute(stage: Stage, env: dict[str, str]) -> None:
    with _stage_descendants():
        _execute(stage, env)


def _execute(stage: Stage, env: dict[str, str]) -> None:
    print(f"verify: {stage.name}: START", flush=True)
    try:
        process = subprocess.Popen(stage.command, cwd=REPO_ROOT, env=env, start_new_session=True)
    except OSError as error:
        raise ProductError(f"verify: {stage.name}: FAIL type=environment: {error}") from error
    try:
        code = process.wait(timeout=stage.timeout)
    except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        # A timed out Python wrapper can still own Cargo, make, or QEMU.
        # Stop the entire stage before allowing another run to reuse its state.
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=5)
        except (ProcessLookupError, subprocess.TimeoutExpired):
            pass
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        if isinstance(error, KeyboardInterrupt):
            raise
        raise ProductError(f"verify: {stage.name}: FAIL type=timeout limit={stage.timeout}s") from error
    if code:
        raise ProductError(f"verify: {stage.name}: FAIL type={stage.failure} exit={code}")
    print(f"verify: {stage.name}: PASS", flush=True)


def environment(tier: str, env: dict[str, str]) -> None:
    execute(Stage("environment-image", "environment", ("dev-env/check-image.sh",), 30), env)
    for command in ("rustup", "cargo", "rustc", "axconfig-gen"):
        if shutil.which(command, path=env.get("PATH")) is None:
            raise ProductError(f"verify: environment: FAIL missing {command}; run scripts/setup-toolchain.sh")
    pin = tomllib.loads((REPO_ROOT / "rust-toolchain.toml").read_text())["toolchain"]
    # rustup run without --install never provisions a missing toolchain.
    execute(Stage("environment-toolchain", "environment", ("rustup", "run", pin["channel"], "rustc", "--version"), 30), env)
    for kind, expected in (("component", pin["components"]), ("target", pin["targets"])):
        result = subprocess.run(("rustup", kind, "list", "--installed", "--toolchain", pin["channel"]), cwd=REPO_ROOT, env=env, capture_output=True, text=True, check=False)
        installed = result.stdout.splitlines()
        if result.returncode or any(not any(line == item or line.startswith(item + "-") for line in installed) for item in expected):
            raise ProductError(f"verify: environment: FAIL missing pinned {kind}; run scripts/setup-toolchain.sh")
    version = subprocess.run(("axconfig-gen", "--version"), cwd=REPO_ROOT, env=env, capture_output=True, text=True, check=False)
    if version.returncode or version.stdout.strip() != "axconfig-gen 0.2.1":
        raise ProductError("verify: environment: FAIL requires axconfig-gen 0.2.1; run scripts/setup-toolchain.sh")
    if tier == "hardware" and not os.access("/dev/kvm", os.R_OK | os.W_OK):
        raise ProductError("verify: environment: UNAVAILABLE /dev/kvm is not readable and writable")


def whitespace(env: dict[str, str]) -> None:
    base = env.get("CI_DIFF_BASE", "")
    if base:
        valid = subprocess.run(("git", "cat-file", "-e", f"{base}^{{commit}}"), cwd=REPO_ROOT, capture_output=True).returncode == 0
        if not valid:
            raise ProductError("verify: whitespace: FAIL type=environment CI_DIFF_BASE commit is unavailable")
        execute(Stage("whitespace-commits", "static", ("git", "diff", "--check", base, "HEAD", "--"), 30), env)
    for name, options in (("working", ()), ("staged", ("--cached",))):
        execute(Stage(f"whitespace-{name}", "static", ("git", "diff", "--check", *options), 30), env)


def verify_cmd(args) -> int:
    state = state_root()
    temporary = state / "test-tmp"
    temporary.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "TMPDIR": str(temporary), "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS") or "2",
           "BR2_JLEVEL": "2"}
    environment(args.tier, env)
    if args.tier != "hardware":
        whitespace(env)
    for stage in plan(args.tier, state):
        execute(stage, env)
    print(f"verify: {args.tier}: PASS", flush=True)
    return 0
