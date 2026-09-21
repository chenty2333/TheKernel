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

from tools.product_state import (
    Artifacts,
    ProductError,
    Variant,
    REPO_ROOT,
    state_root,
)


@dataclass(frozen=True)
class Stage:
    name: str
    failure: str
    command: tuple[str, ...]
    timeout: int


def plan(tier: str, state: Path) -> list[Stage]:
    cli = (sys.executable, str(REPO_ROOT / "tools/thekernel.py"))
    if tier == "hardware":
        # KVM is required here and nowhere else, so the full portable ABI
        # differential (also KVM-only) belongs in this tier rather than daily.
        # `test --suite abi` runs the static linux-abi gate first and then the
        # two-guest comparison; its timeout covers a cold Linux oracle build
        # plus both boots.  verify_cmd strips THEKERNEL_ABI_PROGRAMS for this
        # tier: a tier run is the whole contract set, while running
        # `test --suite abi` by hand still honors the reproduction filter.
        return [
            Stage("cpu-kvm", "test", (*cli, "test", "--suite", "cpu", "--smp", "4", "--accel", "kvm"), 1800),
            Stage("abi-kvm", "test", (*cli, "test", "--suite", "abi", "--smp", "4", "--accel", "kvm"), 3600),
        ]
    stages = [
        Stage("dependency-layers", "static", (sys.executable, "scripts/ci/check_cargo_dependency_layers.py"), 120),
        # The layer gate can only see edges between cargo packages, so inside
        # the one `thekernel` crate a subsystem may reach another without a
        # manifest line changing.  This pins the module-level `use crate::…`
        # edges to config/kernel-module-edges.toml: a new edge fails, a retired
        # one is reported, and the widening that would make `kernel/src/drm`
        # unliftable again cannot happen unnoticed.
        Stage("kernel-module-edges", "static", (sys.executable, "scripts/ci/check_kernel_module_edges.py"), 120),
        # The contract and dispatch tables are a source of truth only while
        # something enforces them.  `test --suite abi` is run by hand, so until
        # this stage existed no verification tier noticed a cell whose status,
        # handler, test binding or explicit-ENOSYS routing had drifted from the
        # kernel.  `all` also materializes the pinned Linux release, which is
        # the network dependency and the reason for the generous timeout.
        # `--final` adds three of the four shrink-only ratchets held in the
        # registry's [ratchet] table (the fourth is the abi-contracts stage's
        # `unbound_programs`): every [[contract]] must carry a
        # `Linux <path>:<line>` citation unless it is in `uncited_contracts`
        # and must have had its errno ordering compared with Linux unless it is
        # in `unreviewed_errno_order`, and an `implemented` cell that declares a
        # validation gap, binds no test, or rests on an unreviewed record must
        # be in `final_static_allowlist`.  No list may grow, so the ledger
        # cannot re-declare a claim gap-free without a citation and a registered
        # differential case behind it.
        Stage("linux-abi", "static", (sys.executable, "scripts/ci/linux_abi_gate.py", "all", "--final"), 900),
        # The ABI registry and its guest C sources describe the same
        # assertion set from two sides; only the KVM tier would otherwise
        # observe a drift, after a full oracle build and two guest boots.  This
        # is the runtime half of the split: it proves a ledger `tests` binding
        # names a program `--suite abi` really boots, which the static gate
        # checks only for cells that claim verified runtime behavior, and that a
        # registered program no cell binds is named by the shrink-only
        # `ratchet.unbound_programs` baseline instead of running unattributed.
        Stage("abi-contracts", "static", (sys.executable, "scripts/ci/check_abi_contracts.py"), 120),
        Stage("graphics-config-seatd", "static", ("scripts/build-graphics-rootfs.sh", "--flavor", "q35-graphics-seatd", "--check"), 120),
        Stage("graphics-config-desktop", "static", ("scripts/build-graphics-rootfs.sh", "--flavor", "q35-software-desktop", "--check"), 120),
        Stage("host", "test", (*cli, "test", "--suite", "host"), 1800),
        Stage("build", "build", (*cli, "build", "--smp", "4", "--memory", "512M"), 1800),
        # A frame that does not fit a task stack is invisible to every other
        # stage: task stacks have no guard page, so the compiler's stack probe
        # writes below the stack instead of faulting (see
        # docs/design/exit-status-race.md).  This reads the release ELF the
        # build stage just produced -- the artifact layout says where it is,
        # and it is the unstripped one, so a finding names the function.
        Stage("stack-frames", "static",
              (sys.executable, "tools/stack_frames.py",
               str(Artifacts(state, Variant(memory="512M")).cargo_elf)), 600),
        Stage("lint", "lint", (*cli, "lint", "--smp", "4", "--memory", "512M"), 1800),
        Stage("guest-tcg", "test", (*cli, "test", "--suite", "guest", "--smp", "4", "--memory", "512M", "--accel", "tcg", "--no-build", "--timeout", "300"), 360),
        # The serial-less acceptance path: the artifacts the build stage just
        # produced, booted on a profile with no virtio-gpu and no serial port,
        # so the firmware framebuffer is the only output the kernel has.  The
        # suite gates its screendump on the first KTAP line and stops there, so
        # it needs neither the graphics rootfs nor the rest of the system suite
        # (whose unrelated `sysv-shm` case is intermittent).
        Stage("firmware-fbcon", "test", (*cli, "test", "--suite", "fbcon", "--smp", "4",
                                         "--memory", "512M", "--accel", "tcg", "--no-build",
                                         "--timeout", "240",
                                         "--graphics-profile", "firmware-fb",
                                         "--screenshot", str(state / "verify-fbcon/console.ppm"),
                                         "--workdir", str(state / "verify-fbcon/run")), 300),
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


#: `github.event.before` for a push that *creates* a branch, which names no
#: previous state rather than an unavailable one.
NULL_COMMIT = "0" * 40


def whitespace(env: dict[str, str]) -> None:
    base = env.get("CI_DIFF_BASE", "")
    # A branch-creating push reports the null commit as its "before". There is
    # no previous state to diff against, so the range that carries meaning is
    # the one this branch adds to the default branch; that is also what a pull
    # request would have compared. Falling back keeps the first push to a new
    # branch covered instead of failing it for an environment reason, and keeps
    # a genuinely unavailable commit an error.
    if base.strip("0") == "" and base:
        base = ""
        merge_base = subprocess.run(("git", "merge-base", "origin/main", "HEAD"),
                                    cwd=REPO_ROOT, capture_output=True, text=True)
        if merge_base.returncode == 0:
            base = merge_base.stdout.strip()
        else:
            print("verify: whitespace: no diff base; a branch-creating push has no "
                  "previous state and origin/main is not fetched here", flush=True)
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
    # The hardware tier runs the full ABI differential.  A caller's
    # THEKERNEL_ABI_PROGRAMS reproduction filter must not silently narrow a
    # tier run; running `test --suite abi` directly still honors it.
    if args.tier == "hardware":
        env.pop("THEKERNEL_ABI_PROGRAMS", None)
    environment(args.tier, env)
    if args.tier != "hardware":
        whitespace(env)
    for stage in plan(args.tier, state):
        execute(stage, env)
    print(f"verify: {args.tier}: PASS", flush=True)
    return 0
