#!/usr/bin/env python3
"""Direct product build, boot, and system-test entry point for TheKernel."""

from __future__ import annotations

import argparse
import math
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Mapping

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from tools.product_state import (
    Artifacts, Variant, ProductError, TARGET, PLATFORM, MEMORY_RE,
    state_root, validate_storage, state_lock, serialized_build, isolated_run,
    artifact_config_stamp, artifact_config_key, artifact_input_key, validate_artifact_config,
    rootfs_stamp_path, rootfs_fingerprint,
)
from tools.verification import verify_cmd
from tools.ktap import COMPLETION_MARKER, KtapError, reject_ktap_skips, validate_ktap_log
from tools.qemu_runner import (
    Interaction,
    ProcessError,
    RunConfig,
    RunLimits,
    RunnerError,
    run,
)
from tools.qemu_runner.model import QmpCheckpoint, QmpColorBlock, QmpControls
from tools.qemu_runner.runner import _validate_output_destinations
from tools.qemu_runner.graphics_benchmark import (
    BENCHMARK_COMPLETE_MARKER,
    benchmark_checkpoints,
    renderer_for_profile,
)
from tools.qemu_runner.graphics_metrics import GraphicsMetricError, enforce_graphics_metrics, parse_graphics_metrics
from tools.qemu_runner.kernel_benchmark import BenchmarkConfig, BenchmarkTarget, run_benchmark_experiment
from tools.qemu_runner.abi_differential import AbiConfig, CONTRACTS as ABI_CONTRACTS, run_abi_differential
from tools.qemu_runner.profiles import BENCHMARK_FAULTS, BENCHMARK_PROFILES, GRAPHICS_PROFILES


PRODUCT_FEATURE = "x86-product"
PRODUCT_MAX_CPUS = 4
SYSTEM_TEST_SHUTDOWN_COMMANDS = "/bin/busybox poweroff -f\nexit\n"
MAX_KERNEL_BYTES = 800 * 1024 * 1024
KERNEL_LOAD_PADDR = 2 * 1024 * 1024
GIB = 1024 * 1024 * 1024
# Q35 reserves the 2--4 GiB PCI hole.  QEMU keeps this amount below 4 GiB
# and places all remaining guest RAM at the high-memory base below.
Q35_PCI_HOLE_LOW_RAM_LIMIT = 2 * GIB
Q35_HIGH_MEMORY_BASE = 4 * GIB
# Keep generated x86_64 physical-memory maps comfortably below the canonical
# 48-bit physical-address envelope while allowing a high-RAM Q35 guest.
X86_64_MAX_MEMORY_BYTES = 1 << 46


def parse_variant(args: argparse.Namespace) -> Variant:
    memory = args.memory.upper()
    if not MEMORY_RE.fullmatch(memory):
        raise ProductError(f"--memory must be a positive K/M/G size: {args.memory}")
    variant = Variant(memory=memory, asid_fast_switch=args.asid_fast_switch,
                      m5_candidate=getattr(args, "m5_candidate", False))
    if variant.memory_bytes <= KERNEL_LOAD_PADDR:
        raise ProductError("--memory must extend beyond the 2 MiB kernel load address")
    if variant.memory_bytes > X86_64_MAX_MEMORY_BYTES:
        raise ProductError(
            f"--memory must not exceed {X86_64_MAX_MEMORY_BYTES // GIB}G on x86_64"
        )
    if args.smp < 1 or args.smp > PRODUCT_MAX_CPUS:
        raise ProductError(
            f"--smp must be an integer between 1 and {PRODUCT_MAX_CPUS}: {args.smp}"
        )
    return variant


def resolve_run_cpus(max_cpus: int, run_cpus: int | None) -> int:
    """Select the QEMU CPU count within the --smp bound."""

    if run_cpus is None:
        return max_cpus
    if run_cpus < 1 or run_cpus > max_cpus:
        raise ProductError(
            f"--run-cpus must be an integer between 1 and --smp ({max_cpus}): {run_cpus}"
        )
    return run_cpus


def positive_timeout(value: str) -> float:
    try:
        timeout = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("timeout must be a number") from error
    if timeout <= 0 or not math.isfinite(timeout):
        raise argparse.ArgumentTypeError("timeout must be a positive finite number")
    return timeout


def command_env(artifacts: Artifacts) -> dict[str, str]:
    target_rustflags = (
        # Keep RustCrypto AES on its scalar backend until the kernel owns the
        # complete SIMD/XSAVE lifecycle for every task and CPU.
        "--cfg aes_force_soft "
        f"-C link-arg=-T{artifacts.linker_script} "
        "-C link-arg=-no-pie -C link-arg=-z -C link-arg=nostart-stop-gc "
        "-C force-frame-pointers -C debuginfo=2 -C strip=none"
    )
    inherited_rustflags = os.environ.get("RUSTFLAGS", "").strip()
    env = {
        **os.environ,
        "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS") or "2",
        "AX_ARCH": "x86_64",
        "AX_PLATFORM": PLATFORM,
        "AX_MODE": "release",
        # Retain useful diagnostics by default. Kernel logs use COM2 and
        # never enter the interactive COM1 terminal.
        "AX_LOG": os.environ.get("AX_LOG") or "info",
        "AX_BACKTRACE": os.environ.get("AX_BACKTRACE") or "n",
        # QEMU user networking's fixed product subnet.  axnet-ng consumes
        # these at compile time and rejects an absent address at boot.
        "AX_IP": "10.0.2.15",
        "AX_GW": "10.0.2.2",
        "SMOLTCP_IFACE_MAX_ADDR_COUNT": "4",
        "AX_TARGET": TARGET,
        "AX_START_BANNER": "n",
        "AX_CONFIG_PATH": str(artifacts.config_path),
        "AX_LINKER_SCRIPT_OUTPUT": str(artifacts.linker_script),
        "CARGO_TARGET_DIR": str(artifacts.cargo_target_dir),
        "RUSTFLAGS": " ".join(
            part for part in (inherited_rustflags, target_rustflags) if part
        ),
    }
    # Cargo gives this variable precedence over RUSTFLAGS, including the
    # product's required linker script. Accept custom flags via RUSTFLAGS only.
    env.pop("CARGO_ENCODED_RUSTFLAGS", None)
    return env


def run_checked(argv: list[str], *, env: dict[str, str] | None = None) -> None:
    print("+", " ".join(argv), file=sys.stderr)
    try:
        completed = subprocess.run(argv, cwd=REPO_ROOT, env=env, check=False)
    except OSError as error:
        raise ProductError(f"could not execute {argv[0]}: {error}") from error
    if completed.returncode:
        raise ProductError(f"command failed ({completed.returncode}): {' '.join(argv)}")


def llvm_objcopy() -> Path:
    """Locate the rust-toolchain LLVM objcopy without a cargo-binutils shim."""

    try:
        completed = subprocess.run(
            ["rustc", "--print", "target-libdir"],
            cwd=REPO_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError as error:
        raise ProductError(f"could not execute rustc to locate llvm-objcopy: {error}") from error
    target_libdir = completed.stdout.strip()
    if completed.returncode or not target_libdir:
        detail = completed.stderr.strip() or f"exit status {completed.returncode}"
        raise ProductError(f"could not locate rust-toolchain llvm-objcopy: {detail}")
    objcopy = Path(target_libdir).resolve().parent / "bin" / "llvm-objcopy"
    if not objcopy.is_file():
        raise ProductError(
            f"rust-toolchain llvm-objcopy is missing: {objcopy}; "
            "ensure rust-toolchain.toml includes the llvm-tools component"
        )
    return objcopy


def generate_config(artifacts: Artifacts) -> None:
    artifacts.config_path.parent.mkdir(parents=True, exist_ok=True)
    generator = shutil.which("axconfig-gen")
    if generator is None:
        raise ProductError("axconfig-gen is required for the x86_64 product build")
    # Generate next to the target and publish only on content change: Cargo
    # tracks the config by mtime, so an identical rewrite would otherwise
    # force a full rebuild of every configuration-dependent crate.
    temp_path = artifacts.config_path.with_name(artifacts.config_path.name + ".tmp")
    run_checked(
        [
            generator,
            str(REPO_ROOT / "config" / "kernel.toml"),
            str(REPO_ROOT / "config" / "x86_64" / "q35-uefi.toml"),
            "-w",
            'arch="x86_64"',
            "-w",
            # One product ELF has four preallocated slots; QEMU's `-smp`
            # chooses how many of them come online for an UP or SMP4 run.
            f"plat.max-cpu-num={PRODUCT_MAX_CPUS}",
            "-w",
            f"plat.phys-memory-size={artifacts.variant.memory_bytes}",
            "-o",
            str(temp_path),
        ]
    )
    if (
        artifacts.config_path.is_file()
        and artifacts.config_path.read_bytes() == temp_path.read_bytes()
    ):
        temp_path.unlink()
        return
    temp_path.replace(artifacts.config_path)


def kernel_features(artifacts: Artifacts) -> str:
    variant = artifacts.variant
    # Keep the product hardware surface explicit and centralized.  The
    # corresponding platform implementations treat unsupported CPUs as a
    # runtime no-op, so a single product ELF still boots on non-Intel and
    # virtualized machines.
    features = [PRODUCT_FEATURE]
    if artifacts.profile == "shell":
        features.append("boot-shell")
    if variant.asid_fast_switch:
        features.append("asid-fast-switch")
    if variant.m5_candidate:
        features.extend(("sched-wake-locality", "io-submit-batch"))
    return " ".join(features)


@serialized_build
def build_kernel(
    artifacts: Artifacts,
    *,
    rootfs: Path | None = None,
    rootfs_transport: str = "module",
) -> None:
    staged_rootfs = rootfs if rootfs is not None else artifacts.rootfs
    if not staged_rootfs.is_file():
        raise ProductError("rootfs is required before building the UEFI ESP")
    if rootfs_transport not in {"module", "drive"}:
        raise ProductError(f"unsupported product rootfs transport: {rootfs_transport}")
    artifact_config_stamp(artifacts, rootfs_transport).unlink(missing_ok=True)
    build_inputs = artifact_input_key(artifacts, rootfs, rootfs_transport)
    generate_config(artifacts)
    env = command_env(artifacts)
    run_checked(
        [
            "cargo",
            "build",
            "--locked",
            "--package",
            "thekernel",
            "--bin",
            "thekernel",
            "--target",
            TARGET,
            "--release",
            "--features",
            kernel_features(artifacts),
        ],
        env=env,
    )
    if not artifacts.cargo_elf.is_file():
        raise ProductError(f"Cargo did not produce the expected ELF: {artifacts.cargo_elf}")
    artifacts.output_dir.mkdir(parents=True, exist_ok=True)
    staged_kernel = artifacts.kernel.with_name(artifacts.kernel.name + ".tmp")
    staged_kernel.unlink(missing_ok=True)
    run_checked(
        [str(llvm_objcopy()), "--strip-all", str(artifacts.cargo_elf), str(staged_kernel)],
        env=env,
    )
    if not staged_kernel.is_file() or staged_kernel.stat().st_size == 0:
        raise ProductError(f"kernel output is empty: {staged_kernel}")
    if staged_kernel.stat().st_size > MAX_KERNEL_BYTES:
        raise ProductError(f"kernel exceeds {MAX_KERNEL_BYTES} bytes: {staged_kernel}")
    low_ram_bytes = min(artifacts.variant.memory_bytes, Q35_PCI_HOLE_LOW_RAM_LIMIT)
    if KERNEL_LOAD_PADDR + staged_kernel.stat().st_size > low_ram_bytes:
        raise ProductError(
            "kernel does not fit below the q35 PCI hole "
            f"({low_ram_bytes // (1024 * 1024)} MiB of low RAM)"
        )
    run_checked(["bash", str(REPO_ROOT / "scripts" / "check-x86-multiboot.sh"), str(staged_kernel)], env=env)
    staged_kernel.replace(artifacts.kernel)
    esp_command = [
        "bash",
        str(REPO_ROOT / "scripts" / "build-x86-uefi-esp.sh"),
        "--kernel",
        str(artifacts.kernel),
        "--output",
        str(artifacts.esp_for_rootfs_transport(rootfs_transport)),
    ]
    if rootfs_transport == "module":
        esp_command.extend((
            "--rootfs", str(staged_rootfs),
            "--grub-config", str(REPO_ROOT / "config" / "x86_64" / "grub.cfg"),
        ))
    else:
        esp_command.extend((
            "--mode", "multiboot-drive",
            "--grub-config", str(REPO_ROOT / "config" / "x86_64" / "grub-drive.cfg"),
        ))
    run_checked(esp_command, env=env)
    if artifact_input_key(artifacts, rootfs, rootfs_transport) != build_inputs:
        raise ProductError("kernel configuration or rootfs changed during build; rebuild before running")
    artifact_config_stamp(artifacts, rootfs_transport).write_text(artifact_config_key(artifacts, rootfs, rootfs_transport) + "\n")


@serialized_build
def build_rootfs(artifacts: Artifacts) -> None:
    artifacts.rootfs.parent.mkdir(parents=True, exist_ok=True)
    fingerprint = rootfs_fingerprint()
    stamp = rootfs_stamp_path(artifacts)
    if (
        artifacts.rootfs.is_file()
        and stamp.is_file()
        and stamp.read_text(encoding="utf-8").strip() == fingerprint
    ):
        print(f"thekernel: rootfs unchanged, reusing {artifacts.rootfs}", file=sys.stderr)
        return
    stamp.unlink(missing_ok=True)
    env = {
        **os.environ,
        "THEKERNEL_SOURCE_CACHE": str(artifacts.root / "source-cache"),
    }
    run_checked(
        [
            "bash",
            str(REPO_ROOT / "scripts" / "build-rootfs.sh"),
            "--arch",
            "x86",
            "--output",
            str(artifacts.rootfs),
        ],
        env=env,
    )
    if rootfs_fingerprint() != fingerprint:
        raise ProductError("rootfs build inputs changed during compilation; rebuild before running")
    stamp.write_text(fingerprint + "\n", encoding="utf-8")


@serialized_build
def lint_kernel(artifacts: Artifacts) -> None:
    generate_config(artifacts)
    run_checked(
        [
            "cargo",
            "clippy",
            "--locked",
            "--package",
            "thekernel",
            "--package",
            "thekernel-kernel",
            "--package",
            "thekernel-linux-process-adapter",
            "--package",
            "thekernel-readiness-adapter",
            "--target",
            TARGET,
            "--release",
            "--features",
            kernel_features(artifacts),
            "--",
            "-D",
            "clippy::correctness",
            "-D",
            "clippy::suspicious",
            "-A",
            "dead-code",
            "-A",
            "clippy::drop-non-drop",
            "-A",
            "clippy::too-many-arguments",
        ],
        env=command_env(artifacts),
    )


@dataclass(frozen=True)
class RunSpec:
    """One product boot: every run_product input as data."""

    accel: str
    timeout: float
    workdir: Path | None
    interactive: bool
    input_after_marker: str | None
    stop_after_marker: str | None
    commands: Path | None
    extra_block: Path | None
    run_cpus: int
    qemu_debug: str | None = None
    gdb: bool = False
    failure_prefixes: tuple[str, ...] = ()
    shutdown_after_marker: bool = False
    completion_after_shutdown: str | None = None
    reject_ktap_skips: bool = False
    graphics_profile: str = "headless"
    rootfs: Path | None = None
    rootfs_transport: str = "module"
    qmp_screenshot: Path | None = None
    qmp_screenshot_after_marker: str | None = None
    qmp_screenshot_size: tuple[int, int] | None = None
    qmp_screenshot_color_blocks: tuple[QmpColorBlock, ...] = ()
    qmp_checkpoints: tuple[QmpCheckpoint, ...] = ()
    qmp_timeout_secs: float = 5.0
    graphics_width: int = 800
    graphics_height: int = 600


@isolated_run
def run_product(artifacts: Artifacts, spec: RunSpec) -> int:
    try:
        selected_esp = artifacts.esp_for_rootfs_transport(spec.rootfs_transport)
    except ProductError:
        raise ProductError("the x86 product supports only module or drive rootfs transport") from None
    if not artifacts.kernel.is_file() or not selected_esp.is_file():
        raise ProductError("kernel and ESP are required; run `thekernel.py build` first")
    selected_rootfs = spec.rootfs if spec.rootfs is not None else artifacts.rootfs
    if not selected_rootfs.is_file():
        raise ProductError("rootfs is required; run `thekernel.py build` first")
    if spec.workdir is not None:
        run_dir = spec.workdir.expanduser().resolve()
    else:
        runs_root = artifacts.root / "runs"
        runs_root.mkdir(parents=True, exist_ok=True)
        run_dir = Path(tempfile.mkdtemp(prefix=f"{artifacts.profile}-", dir=runs_root))
        # Each invocation owns its directory; clean removes completed runs.
    validate_storage(run_dir)
    try:
        run_dir.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ProductError(f"cannot create run directory: {error}") from error
    if spec.gdb:
        print(f"GDB socket: {run_dir / 'gdb.sock'}", file=sys.stderr, flush=True)
    interactive = spec.interactive
    input_after_marker = spec.input_after_marker
    command_path = None
    if spec.shutdown_after_marker:
        if spec.commands is not None or spec.input_after_marker is not None or spec.stop_after_marker is not None:
            raise ProductError("system shutdown-after-marker cannot combine custom marker or command input")
        command_path = run_dir / "system-test-shutdown.commands"
        try:
            command_path.write_text(SYSTEM_TEST_SHUTDOWN_COMMANDS, encoding="utf-8")
        except OSError as error:
            raise ProductError(f"cannot write system-test shutdown command: {error}") from error
        input_after_marker = COMPLETION_MARKER
        interactive = True
    if spec.commands is not None:
        command_path = spec.commands.expanduser().resolve()
        if not command_path.is_file():
            raise ProductError(f"commands file does not exist: {command_path}")
    qmp = QmpControls()
    if spec.qmp_screenshot is not None or spec.qmp_checkpoints:
        qmp = QmpControls(
            socket=run_dir / "graphics-smoke.qmp",
            screenshot=(None if spec.qmp_checkpoints else spec.qmp_screenshot.expanduser().resolve()),
            screenshot_after_marker=(None if spec.qmp_checkpoints else spec.qmp_screenshot_after_marker),
            screenshot_size=(None if spec.qmp_checkpoints else spec.qmp_screenshot_size),
            screenshot_color_blocks=( () if spec.qmp_checkpoints else spec.qmp_screenshot_color_blocks),
            checkpoints=spec.qmp_checkpoints,
            timeout_secs=spec.qmp_timeout_secs,
        )
    result = run(
        RunConfig(
            arch="x86_64",
            kernel=artifacts.kernel,
            rootfs=selected_rootfs,
            rootfs_transport=spec.rootfs_transport,
            esp=selected_esp,
            extra_block=spec.extra_block.expanduser().resolve() if spec.extra_block else None,
            input_path=command_path,
            workdir=run_dir,
            log_path=run_dir / "console.log",
            limits=RunLimits(total_timeout_secs=spec.timeout),
            interaction=Interaction(
                interactive=interactive or command_path is not None,
                input_after_marker=input_after_marker,
                input_line_after_marker=("THEKERNEL_SHELL_READY"
                    if spec.commands is not None and artifacts.profile == "shell" else None),
                stop_after_marker=spec.stop_after_marker,
                failure_prefixes=spec.failure_prefixes,
            ),
            memory=artifacts.variant.memory,
            cpus=spec.run_cpus,
            accel=spec.accel,
            graphics_profile=spec.graphics_profile,
            graphics_width=spec.graphics_width,
            graphics_height=spec.graphics_height,
            extra_args=(("-d", spec.qemu_debug, "-D", str(run_dir / "qemu-debug.log"))
                        if spec.qemu_debug else ()) + (
                ("-gdb", f"unix:{run_dir / 'gdb.sock'},server=on,wait=off",
                 "-action", "reboot=shutdown,shutdown=pause,panic=pause") if spec.gdb else ()),
            qmp=qmp,
        ),
    )
    print(f"qemu-runner exit={result.returncode} log={result.log_path} "
          f"diagnostics={result.diagnostic_log_path}", file=sys.stderr)
    if result.error_message is not None:
        print(f"qemu-runner error={result.error_message}", file=sys.stderr)
    if spec.stop_after_marker is not None:
        if result.intentionally_stopped:
            return 0
        print(
            f"thekernel: guest exited without completion marker: {spec.stop_after_marker}",
            file=sys.stderr,
        )
        return result.returncode if result.returncode != 0 else 1
    if spec.completion_after_shutdown is not None:
        if (not result.guest_clean_shutdown or result.error_message is not None
                or spec.completion_after_shutdown not in result.log_path.read_text(
                    encoding="utf-8", errors="replace").splitlines()):
            print("thekernel: guest did not complete the smoke and shut down cleanly", file=sys.stderr)
            return result.returncode or 1
    if result.guest_clean_shutdown and spec.reject_ktap_skips:
        try:
            validate_ktap_log(result.log_path.read_text(encoding="utf-8", errors="replace"))
        except KtapError as error:
            raise ProductError(str(error)) from error
    return result.returncode


def reject_ktap_skips_in_log(log_path: Path) -> None:
    """Keep a green system-test run distinct from an unsupported test case."""

    try:
        text = log_path.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        raise ProductError(f"cannot read system-test log: {error}") from error
    try:
        reject_ktap_skips(text)
    except KtapError as error:
        raise ProductError(str(error)) from error


def build_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), args.profile)
    rootfs = Path(args.rootfs).expanduser().resolve() if args.rootfs else None
    if rootfs is None:
        build_rootfs(artifacts)
    elif not rootfs.is_file():
        raise ProductError(f"rootfs does not exist: {rootfs}")
    transport = getattr(args, "rootfs_transport", "module")
    build_kernel(artifacts, rootfs=rootfs, rootfs_transport=transport)
    print(artifacts.kernel)
    print(artifacts.esp_for_rootfs_transport(transport))
    return 0


def lint_cmd(args: argparse.Namespace) -> int:
    lint_kernel(Artifacts(state_root(), parse_variant(args), args.profile))
    return 0


# Known generated subdirectories below the state root.  The state root itself
# and anything else under it are user data and are never removed.
CLEAN_STATE_DIRS = ("test-tmp", "runs", "out", "target/thekernel", "source-cache")


def clean_cmd(_args: argparse.Namespace) -> int:
    removed = False
    root = state_root()
    for name in CLEAN_STATE_DIRS:
        path = root / name
        if path.is_dir():
            shutil.rmtree(path)
            print(path)
            removed = True
    if not removed:
        print("thekernel: nothing to clean")
    return 0


@serialized_build
def build_desktop_rootfs(artifacts: Artifacts) -> Path:
    """Use the graphics builder's incremental Buildroot output for the desktop."""
    output = artifacts.root / "graphics-desktop"
    source = Path(os.environ.get("THEKERNEL_BUILDROOT_DIR") or
                  str(artifacts.root / "buildroot" / "source")).expanduser().resolve()
    downloads = Path(os.environ.get("THEKERNEL_GRAPHICS_DL_DIR") or
                     str(artifacts.root / "graphics-downloads")).expanduser().resolve()
    for path in (output, source, downloads):
        validate_storage(path)
    command = [
        str(REPO_ROOT / "scripts" / "build-graphics-rootfs.sh"),
        "--flavor", "q35-software-desktop", "--output", str(output),
        "--buildroot-dir", str(source), "--fetch-buildroot",
        "--download-dir", str(downloads),
    ]
    host_deps = Path(os.environ.get("THEKERNEL_GRAPHICS_HOST_DEPS_DIR") or
                     str(artifacts.root / "graphics-host-deps")).expanduser().resolve()
    if (host_deps / "bin" / "perl").is_file():
        command.extend(["--host-deps-dir", str(host_deps)])
    run_checked(command)
    return output / "images" / "rootfs.ext2"


def run_gui_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), "system")
    resolve_run_cpus(args.smp, args.run_cpus)
    if not args.rootfs:
        rootfs = artifacts.root / "graphics-desktop" / "images" / "rootfs.ext2"
        if not args.no_build:
            rootfs = build_desktop_rootfs(artifacts)
        args.rootfs = str(rootfs)
    return run_cmd(args)


def run_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), args.profile)
    run_cpus = resolve_run_cpus(args.smp, args.run_cpus)
    rootfs = Path(args.rootfs).expanduser().resolve() if args.rootfs else None
    if rootfs is not None and not rootfs.is_file():
        raise ProductError(f"rootfs does not exist: {rootfs}")
    if not args.no_build:
        if rootfs is None:
            build_rootfs(artifacts)
        build_kernel(artifacts, rootfs=rootfs, rootfs_transport=args.rootfs_transport)
    input_after_marker = args.input_after_marker
    if (
        args.commands
        and input_after_marker is None
        and artifacts.profile == "shell"
    ):
        input_after_marker = "THEKERNEL_SHELL_READY"
    return run_product(
        artifacts,
        RunSpec(
            accel=args.accel,
            timeout=args.timeout,
            qemu_debug=getattr(args, "qemu_debug", None),
            gdb=args.gdb,
            workdir=Path(args.workdir) if args.workdir else None,
            interactive=args.interactive,
            graphics_profile=args.graphics_profile,
            input_after_marker=input_after_marker,
            stop_after_marker=args.stop_after_marker,
            commands=Path(args.commands) if args.commands else None,
            extra_block=Path(args.extra_block) if args.extra_block else None,
            rootfs=rootfs,
            rootfs_transport=args.rootfs_transport,
            run_cpus=run_cpus,
        ),
    )


def system_test_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), "system")
    run_cpus = resolve_run_cpus(args.smp, args.run_cpus)
    if not args.no_build:
        build_rootfs(artifacts)
        build_kernel(artifacts)
    return run_product(
        artifacts,
        RunSpec(
            accel=args.accel,
            timeout=args.timeout,
            qemu_debug=getattr(args, "qemu_debug", None),
            workdir=Path(args.workdir) if args.workdir else None,
            interactive=False,
            graphics_profile="headless",
            input_after_marker=None,
            stop_after_marker=None,
            commands=None,
            extra_block=None,
            shutdown_after_marker=True,
            reject_ktap_skips=not args.allow_skip,
            rootfs_transport="module",
            run_cpus=run_cpus,
        ),
    )


@dataclass(frozen=True)
class SmokeFlavor:
    """One graphics smoke flavor's marker and pixel oracle as data."""

    marker: str
    profile_markers: Mapping[str, str] = field(default_factory=dict)
    failure_markers: tuple[str, ...] = ()
    screenshot_size: tuple[int, int] | None = None
    screenshot_color_blocks: tuple[QmpColorBlock, ...] = ()
    qmp_timeout_secs: float = 120.0


SMOKE_FLAVORS = {
    # Pure boot smoke: the headless ABI guest has no pixel oracle.
    "headless-abi-smoke": SmokeFlavor(marker="THEKERNEL_GRAPHICS_ABI_SMOKE_READY"),
    "q35-graphics-seatd": SmokeFlavor(
        marker="THEKERNEL_Q35_WESTON_READY",
        profile_markers={
            "virgl-interactive": "THEKERNEL_Q35_VIRGL_READY",
            "venus-interactive": "THEKERNEL_Q35_VENUS_READY",
        },
        screenshot_size=(800, 600),
        screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (255, 0, 0)),),
    ),
    "q35-graphics-logind": SmokeFlavor(
        marker="THEKERNEL_Q35_SWAY_READY",
        failure_markers=("THEKERNEL_Q35_LOGIND_CYCLE",),
        qmp_timeout_secs=900.0,
    ),
}


def _xwayland_smoke_checkpoints(screenshot: Path) -> tuple[QmpCheckpoint, ...]:
    # The rootless Xwayland client publishes this marker only after its
    # mapped, drawn, resized, focused, clipboard-owning foreground window
    # is stacked above the background window.  Inject directly into the
    # virtio devices while it records the resulting X11 key/pointer events.
    return (
        QmpCheckpoint(
            input_after_marker="THEKERNEL_Q35_XWAYLAND_EVENT_READY",
            input_events=((
                {"type": "abs", "data": {"axis": "x", "value": 180}},
                {"type": "abs", "data": {"axis": "y", "value": 160}},
                {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},
                {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "a"}}},
            ),),
            screenshot=screenshot.with_name(f"{screenshot.stem}-xwayland{''.join(screenshot.suffixes)}"),
            screenshot_after_marker="THEKERNEL_Q35_XWAYLAND_POINTER_EVENT",
            screenshot_size=(800, 600),
            screenshot_color_blocks=(QmpColorBlock(160, 150, 32, 32, (255, 32, 64)),),
        ),
    )


def _logind_smoke_checkpoints(marker: str, screenshot: Path) -> tuple[QmpCheckpoint, ...]:
    # The guest does not enter its A -> B -> A logind handoff until this
    # initial pointer checkpoint has reached Alice's persistent Wayland
    # client.  The final checkpoint observes the same session restored
    # after all revocation/ACL transitions.
    checkpoints: list[QmpCheckpoint] = [
        QmpCheckpoint(
            input_after_marker=marker,
            input_events=((
                {"type": "abs", "data": {"axis": "x", "value": 320}},
                {"type": "abs", "data": {"axis": "y", "value": 240}},
            ),),
            screenshot=screenshot,
            screenshot_after_marker="THEKERNEL_Q35_SWAY_ALICE_POINTER_REPAINT",
            screenshot_size=(800, 600),
            screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (0, 102, 255)),),
        ),
    ]
    for cycle in range(1, 2):
        checkpoints.append(QmpCheckpoint(
            input_after_marker=f"THEKERNEL_Q35_LOGIND_CYCLE_BOB_POINTER_READY_{cycle:03d}",
            input_events=((
                {"type": "rel", "data": {"axis": "x", "value": 1}},
            ),),
        ))
        checkpoints.append(QmpCheckpoint(
            input_after_marker=f"THEKERNEL_Q35_LOGIND_CYCLE_ALICE_KEY_READY_{cycle:03d}",
            input_events=((
                {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},
                {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "a"}}},
            ),),
        ))
    checkpoints.append(
        QmpCheckpoint(
            input_after_marker="THEKERNEL_Q35_LOGIND_CYCLES_COMPLETE",
            screenshot=screenshot.with_name(f"{screenshot.stem}-logind-cycles{''.join(screenshot.suffixes)}"),
            screenshot_after_marker="THEKERNEL_Q35_LOGIND_CYCLES_COMPLETE",
            screenshot_size=(800, 600),
            screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (0, 102, 255)),),
        )
    )
    return tuple(checkpoints)


def _seatd_smoke_checkpoints(
    marker: str,
    screenshot: Path,
    size: tuple[int, int],
    blocks: tuple[QmpColorBlock, ...],
) -> tuple[QmpCheckpoint, ...]:
    # QMP drives the virtio keyboard/tablet devices directly. Repaint
    # callbacks gate each action, but can precede scanout: the controller
    # polls the pixel oracle to verify presentation before advancing.
    # The original screenshot remains the initial red SHM frame; these
    # files carry the later input checkpoints.
    return (
        QmpCheckpoint(
            input_after_marker=marker,
            screenshot=screenshot,
            screenshot_after_marker=marker,
            screenshot_size=size,
            screenshot_color_blocks=blocks,
        ),
        QmpCheckpoint(
            input_after_marker="THEKERNEL_Q35_WAYLAND_INPUT_READY",
            input_events=((
                {"type": "abs", "data": {"axis": "x", "value": 320}},
                {"type": "abs", "data": {"axis": "y", "value": 240}},
            ),),
            screenshot=screenshot.with_name(f"{screenshot.stem}-tablet{''.join(screenshot.suffixes)}"),
            screenshot_after_marker="THEKERNEL_Q35_WAYLAND_POINTER_REPAINT",
            screenshot_size=size,
            screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (0, 102, 255)),),
        ),
        QmpCheckpoint(
            input_after_marker="THEKERNEL_Q35_WAYLAND_POINTER_REPAINT",
            input_events=((
                {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},
                {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "a"}}},
            ),),
            screenshot=screenshot.with_name(f"{screenshot.stem}-key{''.join(screenshot.suffixes)}"),
            screenshot_after_marker="THEKERNEL_Q35_WAYLAND_KEY_REPAINT",
            screenshot_size=size,
            screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (255, 0, 255)),),
        ),
        QmpCheckpoint(
            input_after_marker="THEKERNEL_Q35_WAYLAND_KEY_REPAINT",
            input_events=((
                {"type": "rel", "data": {"axis": "x", "value": 8}},
                {"type": "btn", "data": {"down": True, "button": "left"}},
                {"type": "btn", "data": {"down": False, "button": "left"}},
            ),),
            screenshot=screenshot.with_name(f"{screenshot.stem}-pointer{''.join(screenshot.suffixes)}"),
            screenshot_after_marker="THEKERNEL_Q35_WAYLAND_BUTTON_REPAINT",
            screenshot_size=size,
            screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (0, 204, 102)),),
        ),
    )


def graphics_smoke_cmd(args: argparse.Namespace) -> int:
    if args.graphics_profile == "venus-interactive":
        # The Phase-7 reference topology is intentionally fixed.  Do not
        # silently run Venus with the smaller software/legacy-virgl guest.
        args.smp = 4
        args.memory = "4G"
    artifacts = Artifacts(state_root(), parse_variant(args), args.profile)
    rootfs = Path(args.rootfs).expanduser().resolve()
    screenshot = Path(args.screenshot).expanduser().resolve()
    if not rootfs.is_file():
        raise ProductError(f"rootfs does not exist: {rootfs}")
    if args.graphics_profile == "virgl-headless":
        raise ProductError(
            "virgl-headless graphics smoke is unsupported: its EGL-headless "
            "display has no QMP pixel-oracle surface; use virgl-interactive"
        )
    if (
        args.graphics_profile in {"virgl-interactive", "venus-interactive"}
        and args.flavor not in {"q35-graphics-seatd", "q35-graphics-logind"}
    ):
        raise ProductError(
            f"{args.graphics_profile} graphics smoke requires "
            "--flavor q35-graphics-seatd or q35-graphics-logind"
        )
    if not args.no_build:
        build_kernel(artifacts, rootfs=rootfs, rootfs_transport="drive")
    descriptor = SMOKE_FLAVORS[args.flavor]
    marker = descriptor.profile_markers.get(args.graphics_profile, descriptor.marker)
    if args.flavor == "q35-graphics-logind":
        checkpoints = _logind_smoke_checkpoints(marker, screenshot)
    elif args.flavor == "q35-graphics-seatd" and args.graphics_profile == "virgl-interactive":
        checkpoints = _xwayland_smoke_checkpoints(screenshot)
    elif args.flavor == "q35-graphics-seatd":
        size = descriptor.screenshot_size
        assert size is not None
        checkpoints = _seatd_smoke_checkpoints(
            marker, screenshot, size, descriptor.screenshot_color_blocks
        )
    else:
        checkpoints = ()
    software_exit = args.flavor == "q35-graphics-seatd" and args.graphics_profile == "headless"
    if software_exit:
        # Checkpoints execute sequentially: F12 follows the final pixel oracle.
        checkpoints += (QmpCheckpoint(
            input_after_marker="THEKERNEL_Q35_WAYLAND_BUTTON_REPAINT",
            input_events=((
                {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "f12"}}},
                {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "f12"}}},
            ),),
        ),)
    return run_product(
        artifacts,
        RunSpec(
            accel=args.accel,
            timeout=args.timeout,
            qemu_debug=getattr(args, "qemu_debug", None),
            gdb=getattr(args, "gdb", False),
            workdir=Path(args.workdir) if args.workdir else None,
            interactive=False,
            graphics_profile=args.graphics_profile,
            input_after_marker=None,
            stop_after_marker=None if software_exit else marker,
            completion_after_shutdown="THEKERNEL_Q35_SOFTWARE_SMOKE_COMPLETE" if software_exit else None,
            failure_prefixes=tuple(name + " state=FAIL" for name in
                dict.fromkeys((descriptor.marker, marker, *descriptor.profile_markers.values(),
                               *descriptor.failure_markers))),
            commands=None,
            extra_block=None,
            rootfs=rootfs,
            rootfs_transport="drive",
            qmp_screenshot=screenshot,
            qmp_screenshot_after_marker=marker,
            qmp_screenshot_size=descriptor.screenshot_size,
            qmp_screenshot_color_blocks=descriptor.screenshot_color_blocks,
            qmp_checkpoints=checkpoints,
            qmp_timeout_secs=descriptor.qmp_timeout_secs,
            run_cpus=args.smp,
        ),
    )


def graphics_benchmark_cmd(args: argparse.Namespace) -> int:
    """Run the fixed 4K/KVM graphics protocol and gate its serial metrics."""

    if args.accel != "kvm":
        raise ProductError("graphics benchmark requires KVM; TCG is correctness-only")
    args.smp, args.memory = 4, "4G"
    artifacts = Artifacts(state_root(), parse_variant(args), "system")
    rootfs = Path(args.rootfs).expanduser().resolve()
    if not rootfs.is_file():
        raise ProductError(f"rootfs does not exist: {rootfs}")
    run_dir = Path(args.workdir).expanduser().resolve()
    oracle_log = Path(args.linux_oracle_log).expanduser().resolve()
    # Validate aliases before removing the prior generated result: a repeated
    # failed run must not display old success, or overwrite its Linux oracle.
    outputs = [("graphics metrics", run_dir / "graphics-metrics.json"),
               ("console log", run_dir / "console.log"),
               ("QMP socket", run_dir / "graphics-smoke.qmp"),
               ("firmware vars", run_dir / "firmware" / "OVMF_VARS.fd")]
    if getattr(args, "qemu_debug", None):
        outputs.append(("QEMU debug log", run_dir / "qemu-debug.log"))
    try:
        checked = _validate_output_destinations(tuple(outputs), protected_paths=(
            rootfs, oracle_log, artifacts.kernel, artifacts.drive_esp,
        ))
        metrics_path = checked["graphics metrics"]
        metrics_path.unlink(missing_ok=True)
    except (RunnerError, OSError) as error:
        raise ProductError(str(error)) from error
    if not args.no_build:
        build_kernel(artifacts, rootfs=rootfs, rootfs_transport="drive")
    result = run_product(
        artifacts,
        RunSpec(
            accel="kvm",
            timeout=args.timeout,
            qemu_debug=getattr(args, "qemu_debug", None),
            workdir=run_dir,
            interactive=False,
            graphics_profile=args.graphics_profile,
            input_after_marker=None,
            stop_after_marker=BENCHMARK_COMPLETE_MARKER,
            commands=None,
            extra_block=None,
            rootfs=rootfs,
            # Graphics boots the same rootfs snapshot from the sole virtio-blk
            # device as the Linux oracle, so their Q35/VirtIO topology is equal.
            rootfs_transport="drive",
            qmp_checkpoints=benchmark_checkpoints(args.fault),
            qmp_timeout_secs=300.0,
            graphics_width=3840,
            graphics_height=2160,
            run_cpus=args.smp,
        ),
    )
    if result:
        return result
    log = run_dir / "console.log"
    try:
        current = parse_graphics_metrics(log)
        oracle = parse_graphics_metrics(oracle_log)
        expected_renderer = renderer_for_profile(args.graphics_profile)
        enforce_graphics_metrics(oracle, expected_renderer=expected_renderer, expected_fault=args.fault or "none")
        enforce_graphics_metrics(
            current,
            oracle,
            expected_renderer=expected_renderer,
            expected_fault=args.fault or "none",
        )
    except GraphicsMetricError as error:
        raise ProductError(str(error)) from error
    metrics_path.write_text(current.json() + "\n", encoding="utf-8")
    print(current.json())
    return 0


def add_variant_arguments(parser: argparse.ArgumentParser, *, profiles: bool = True) -> None:
    parser.add_argument("--smp", type=int, default=4)
    parser.add_argument("--memory", default="1G")
    parser.add_argument("--asid-fast-switch", action="store_true")
    parser.add_argument("--m5-candidate", action="store_true",
                        help="build or validate the experimental scheduler/I/O candidate in separate artifact paths")
    if profiles:
        parser.add_argument(
            "--profile",
            choices=("system", "shell"),
            default="system",
        )


def add_run_arguments(parser: argparse.ArgumentParser) -> None:
    add_variant_arguments(parser)
    parser.add_argument(
        "--run-cpus",
        type=int,
        help="boot the --smp artifact with this many QEMU CPUs (1 through --smp)",
    )
    parser.add_argument(
        "--no-build",
        action="store_true",
        help="boot existing kernel, ESP, and rootfs artifacts without rebuilding",
    )
    parser.add_argument("--accel", choices=("tcg", "kvm"), default="tcg")
    parser.add_argument("--timeout", type=positive_timeout, default=300.0)
    parser.add_argument("--workdir")
    parser.add_argument("--qemu-debug", help="QEMU -d categories; write workdir/qemu-debug.log")
    parser.add_argument("--gdb", action="store_true",
                        help="serve workdir/gdb.sock; pause on guest shutdown/reboot/panic for inspection")
    parser.add_argument("--rootfs-transport", choices=("module", "drive"), default="module")
    parser.add_argument("--interactive", action="store_true")
    parser.add_argument(
        "--graphics-profile",
        choices=tuple(GRAPHICS_PROFILES),
        default="headless",
        help="select the explicit QEMU display and virtio-gpu topology",
    )
    parser.add_argument("--commands", help="forward this command file to the guest in this process")
    parser.add_argument("--input-after-marker")
    parser.add_argument("--stop-after-marker")
    parser.add_argument("--extra-block")
    parser.add_argument(
        "--rootfs",
        help="boot this existing rootfs image instead of the standard generated rootfs",
    )


GRAPHICS_FLAVORS_ENV = REPO_ROOT / "config" / "graphics" / "flavors.env"


def graphics_flavor_manifest() -> dict[str, tuple[str, ...]]:
    """Read the dual-parsed graphics flavor manifest.

    config/graphics/flavors.env is sourced by bash in the rootfs builder, so
    it holds only plain KEY=VALUE assignments; values with spaces are
    double-quoted and lists are space-separated.
    """

    try:
        lines = GRAPHICS_FLAVORS_ENV.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise ProductError(f"cannot read graphics flavor manifest: {error}") from error
    manifest: dict[str, tuple[str, ...]] = {}
    for line in lines:
        key, separator, value = line.strip().partition("=")
        if not key or key.startswith("#"):
            continue
        if not separator:
            raise ProductError(f"invalid graphics flavor manifest line: {line.strip()}")
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] == '"':
            value = value[1:-1]
        manifest[key] = tuple(value.split())
    return manifest


def graphics_flavor_list(key: str) -> tuple[str, ...]:
    flavors = graphics_flavor_manifest().get(key, ())
    if not flavors:
        raise ProductError(f"graphics flavor manifest lacks a {key} list")
    return flavors


def graphics_flavors() -> tuple[str, ...]:
    return graphics_flavor_list("FLAVORS")


def graphics_smoke_flavors() -> tuple[str, ...]:
    return graphics_flavor_list("SMOKE_FLAVORS")


def add_graphics_smoke_arguments(parser: argparse.ArgumentParser) -> None:
    add_variant_arguments(parser)
    parser.add_argument("--rootfs", help="existing graphics rootfs.ext2 image")
    parser.add_argument("--flavor", choices=graphics_smoke_flavors(), default="headless-abi-smoke")
    parser.add_argument("--screenshot", help="QMP screendump PPM output path")
    parser.add_argument("--accel", choices=("tcg", "kvm"), default="tcg")
    parser.add_argument("--timeout", type=positive_timeout, default=300.0)
    parser.add_argument("--workdir")
    parser.add_argument(
        "--no-build",
        action="store_true",
        help="boot existing kernel and ESP artifacts without rebuilding them",
    )
    parser.add_argument(
        "--graphics-profile",
        # virgl-headless stays rejected at runtime: its EGL-headless display
        # has no QMP pixel-oracle surface for a smoke run.
        choices=tuple(profile for profile in GRAPHICS_PROFILES if profile != "virgl-headless"),
        default="headless",
        help="QEMU display topology; headless uses QMP screendump without a host window",
    )


def add_graphics_benchmark_arguments(parser: argparse.ArgumentParser) -> None:
    add_variant_arguments(parser, profiles=False)
    parser.add_argument("--rootfs", help="q35-graphics-benchmark rootfs.ext2")
    parser.add_argument("--accel", choices=("kvm",), default="kvm")
    parser.add_argument("--timeout", type=positive_timeout, default=1800.0)
    parser.add_argument("--fault", choices=tuple(sorted(BENCHMARK_FAULTS)))
    parser.add_argument("--workdir")
    parser.add_argument("--linux-oracle-log")
    parser.add_argument(
        "--graphics-profile",
        choices=BENCHMARK_PROFILES,
        default="virgl-interactive",
    )



def component_host_test_command(package: dict) -> list[str]:
    settings = package.get("metadata", {}).get("thekernel", {}).get("host-test", {})
    command = ["cargo", "test", "--locked", "-p", package["name"]]
    features = settings.get("features", [])
    if not isinstance(features, list) or any(not isinstance(feature, str) or not feature for feature in features):
        raise ProductError(f"invalid host-test features for {package['name']}")
    if features:
        command.extend(("--features", ",".join(features)))
    if not settings.get("default-features", True):
        command.append("--no-default-features")
    if settings.get("all-targets", False):
        command.append("--all-targets")
    target = settings.get("target", "x86_64-unknown-linux-gnu")
    if target is not None:
        if target != "x86_64-unknown-linux-gnu":
            raise ProductError(f"unsupported host-test target for {package['name']}: {target}")
        command.extend(("--target", target))
    return command


def host_test_cmd() -> int:
    test_tmp = state_root() / "test-tmp"
    test_tmp.mkdir(parents=True, exist_ok=True)
    run_checked([sys.executable, "-m", "unittest", "discover", "-s", "tests", "-t", "."], env={**os.environ, "TMPDIR": str(test_tmp)})
    env = {**os.environ, "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS") or "2",
           "RUST_TEST_THREADS": "1", "TMPDIR": str(test_tmp),
           "CARGO_TARGET_DIR": str(state_root() / "target" / "thekernel" / "host")}
    # Product build flags can override target-specific host flags, including
    # the percpu linker script. Host fixtures must always use the hosted ABI.
    for variable in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET",
                     "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"):
        env.pop(variable, None)
    env.update({"CC": "gcc", "CXX": "g++", "AR": "ar", "AS": "as",
                "OBJCOPY": "objcopy", "OBJDUMP": "objdump", "SIZE": "size",
                "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER":
                    str(REPO_ROOT / "scripts/ci/host-test-linker.sh")})
    metadata = subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1", "--no-deps"],
                              cwd=REPO_ROOT, env=env, capture_output=True, text=True, check=False)
    if metadata.returncode:
        raise ProductError(metadata.stderr.strip() or "cannot discover component host tests")
    packages = json.loads(metadata.stdout)["packages"]
    selected = []
    for package in packages:
        settings = package.get("metadata", {}).get("thekernel", {})
        if (settings.get("layer") in {"mechanism", "linux_abi"}
                or settings.get("host-test", {}).get("selected", False)
                or package["name"] in {"thekernel-readiness-adapter", "thekernel-linux-process-adapter"}):
            selected.append(package)
    # Separate invocations preserve declared component test features; a
    # workspace-wide union changes scheduler and platform semantics.
    for package in sorted(selected, key=lambda item: item["name"]):
        run_checked(component_host_test_command(package), env=env)
    env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"] = (
        f"-C link-arg=-T{REPO_ROOT / 'crates/ax/thekernel-scope-local/percpu.x'}")
    run_checked(["cargo", "test", "--locked", "--manifest-path", "kernel/Cargo.toml",
                 "--tests", "--features", "bpf,perf-sampling,axtask/test", "--target",
                 "x86_64-unknown-linux-gnu", "--", "--test-threads=1"], env=env)
    return 0


def test_cmd(args: argparse.Namespace) -> int:
    suites = ("host", "guest", "abi", "graphics", "cpu") if args.suite == "all" else (args.suite,)
    for suite in suites:
        if suite == "host":
            host_test_cmd()
        elif suite == "guest":
            result = system_test_cmd(args)
            if result:
                return result
        elif suite == "abi":
            run_checked([sys.executable, "scripts/ci/linux_abi_gate.py", "all"])
            result = abi_test_cmd(args)
            if result:
                return result
        elif suite == "graphics":
            if not args.rootfs or not args.screenshot:
                raise ProductError("graphics suite requires --rootfs and --screenshot")
            result = graphics_smoke_cmd(args)
            if result:
                return result
        elif suite == "cpu":
            result = cpu_test_cmd(args)
            if result:
                return result
    return 0


def guest_tool_run(args: argparse.Namespace, command: str, marker: str, cpus: int) -> tuple[int, Path]:
    artifacts = Artifacts(state_root(), parse_variant(args), "shell")
    if not args.no_build:
        build_rootfs(artifacts)
        build_kernel(artifacts)
    runs = Path(args.workdir).expanduser().resolve() if args.workdir else state_root() / "runs"
    validate_storage(runs)
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="suite-", dir=runs))
    commands = directory / "commands"
    commands.write_text(f"{command} && echo {marker}\n/bin/busybox poweroff -f\nexit\n", encoding="utf-8")
    result = run_product(artifacts, RunSpec(
        accel="kvm", timeout=args.timeout, workdir=directory, interactive=False,
        input_after_marker="THEKERNEL_SHELL_READY", stop_after_marker=None,
        commands=commands, extra_block=None, run_cpus=cpus,
        qemu_debug=args.qemu_debug))
    log = directory / "console.log"
    if result == 0 and marker not in log.read_text(encoding="utf-8", errors="replace").splitlines():
        raise ProductError(f"guest tool failed or missed completion marker; log={log}")
    return result, log


_CPU_VISIBLE_FIELDS = frozenset(
    ("hypervisor", "apic", "pcid", "invpcid", "xsave", "pku", "cet_ss")
)
_CPU_ENABLED_FIELDS = frozenset(
    ("apic", "apic_software", "x2apic", "pcid", "osxsave", "xcr0", "pke", "cet_cr4", "syscall")
)


def _validate_cpu_capability_reports(text: str, cpus: int, log: Path | None = None) -> None:
    """Validate the per-CPU boot capability transcript and its implications."""

    def fail(message: str) -> None:
        suffix = f"; log={log}" if log is not None else ""
        raise ProductError(f"invalid per-CPU capability report: {message}{suffix}")

    if cpus < 1:
        fail(f"invalid CPU count {cpus}")

    def parse(category: str, required: frozenset[str]) -> dict[int, dict[str, str]]:
        records = re.findall(
            rf"^THEKERNEL_CPU_{category} cpu=(\d+) (.*)$", text, re.MULTILINE
        )
        cpu_ids = [int(cpu) for cpu, _ in records]
        if sorted(cpu_ids) != list(range(cpus)):
            fail(f"missing or duplicate per-CPU {category} capability report")
        parsed: dict[int, dict[str, str]] = {}
        for cpu_text, values in records:
            cpu = int(cpu_text)
            fields: dict[str, str] = {}
            for field in values.split():
                if "=" not in field:
                    fail(f"CPU {cpu} has malformed {category} field {field!r}")
                key, value = field.split("=", 1)
                if not key or not value or key in fields:
                    fail(f"CPU {cpu} has malformed or duplicate {category} field {field!r}")
                fields[key] = value
            missing = sorted(required - fields.keys())
            if missing:
                fail(f"CPU {cpu} is missing {category} fields: {', '.join(missing)}")
            for key in required - {"xcr0"}:
                if fields[key] not in {"0", "1"}:
                    fail(f"CPU {cpu} has non-boolean {category} field {key}={fields[key]!r}")
            if "xcr0" in required:
                try:
                    xcr0 = int(fields["xcr0"], 0)
                except ValueError:
                    fail(f"CPU {cpu} has invalid xcr0={fields['xcr0']!r}")
                if xcr0 < 0:
                    fail(f"CPU {cpu} has negative xcr0={fields['xcr0']!r}")
            parsed[cpu] = fields
        return parsed

    visible = parse("VISIBLE", _CPU_VISIBLE_FIELDS)
    enabled = parse("ENABLED", _CPU_ENABLED_FIELDS)
    for cpu in range(cpus):
        v = visible[cpu]
        e = enabled[cpu]
        xcr0 = int(e["xcr0"], 0)

        required_enabled = [
            key for key in ("apic", "apic_software", "syscall") if e[key] != "1"
        ]
        if required_enabled:
            fail(f"CPU {cpu} lacks required enabled state: {', '.join(required_enabled)}")

        # Privileged state may be disabled despite hardware support being
        # visible, but an enabled state must have the corresponding contract.
        implications = (
            ("apic", "apic"),
            ("pcid", "pcid"),
            ("osxsave", "xsave"),
            ("cet_cr4", "cet_ss"),
        )
        for enabled_field, visible_field in implications:
            if e[enabled_field] == "1" and v[visible_field] != "1":
                fail(
                    f"CPU {cpu} has enabled {enabled_field}=1 without "
                    f"visible {visible_field}=1"
                )
        if e["apic_software"] == "1" and e["apic"] != "1":
            fail(f"CPU {cpu} has apic_software=1 while apic=0")
        if e["osxsave"] == "0" and xcr0 != 0:
            fail(f"CPU {cpu} reports xcr0={e['xcr0']} while osxsave=0")
        if xcr0 != 0 and e["osxsave"] != "1":
            fail(f"CPU {cpu} reports nonzero xcr0 without osxsave=1")
        if e["osxsave"] == "1" and xcr0 & 0x3 != 0x3:
            fail(f"CPU {cpu} has osxsave=1 without x87/SSE state in xcr0")
        if e["pke"] == "1":
            if v["pku"] != "1":
                fail(f"CPU {cpu} has pke=1 without visible pku=1")
            if e["osxsave"] != "1" or not xcr0 & (1 << 9):
                fail(f"CPU {cpu} has pke=1 without OSXSAVE PKRU state in xcr0")


def cpu_test_cmd(args: argparse.Namespace) -> int:
    if args.accel != "kvm":
        raise ProductError("CPU suite requires --accel kvm")
    if args.smp < 4:
        raise ProductError("CPU suite requires --smp 4 for its 1/4 vCPU matrix")
    run_checked(["lscpu"])
    for cpus in (1, 4):
        result, log = guest_tool_run(args,
            f"/opt/thekernel-tests/bin/thekernel-cpu-smoke --expected-cpus {cpus} --require-kvm",
            "THEKERNEL_CPU_EXIT_ZERO", cpus)
        if result:
            return result
        text = log.read_text()
        diagnostic_log = log.with_name("kernel.log")
        try:
            diagnostics = diagnostic_log.read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            raise ProductError(f"cannot read CPU diagnostics: {diagnostic_log}: {error}") from error
        _validate_cpu_capability_reports(diagnostics, cpus, diagnostic_log)
        try:
            validate_ktap_log(text.replace("# THEKERNEL_CPU_TEST_COMPLETE", COMPLETION_MARKER))
        except KtapError as error:
            raise ProductError(str(error)) from error
    return 0


def abi_test_cmd(args: argparse.Namespace) -> int:
    if args.accel != "kvm":
        raise ProductError("ABI differential requires --accel kvm")
    artifacts = Artifacts(state_root(), parse_variant(args), "shell")
    # --rootfs belongs to the graphics suite. ABI always uses the product
    # shell rootfs, including when test --suite all also exercises graphics.
    rootfs = artifacts.rootfs
    if not args.no_build:
        build_rootfs(artifacts)
        build_kernel(artifacts, rootfs=rootfs, rootfs_transport="drive")
    linux_kernel = Path(args.linux_kernel).expanduser().resolve() if args.linux_kernel else None
    if linux_kernel is None:
        if args.no_build:
            raise ProductError("--no-build ABI differential requires --linux-kernel")
        completed = subprocess.run(
            ["bash", str(REPO_ROOT / "scripts/build-linux-oracle.sh"), "--jobs",
             os.environ.get("CARGO_BUILD_JOBS", "2")],
            cwd=REPO_ROOT, stdout=subprocess.PIPE, text=True, check=False,
        )
        if completed.returncode:
            raise ProductError("Linux oracle build failed")
        linux_kernel = Path(completed.stdout.strip()).resolve()
    if not linux_kernel.is_file():
        raise ProductError(f"Linux oracle kernel is missing: {linux_kernel}")
    output = Path(args.workdir).expanduser().resolve() if args.workdir else state_root() / "runs"
    validate_storage(output)
    linux_esp = state_root() / "out/linux-7.2.3/abi.esp"
    if not args.no_build:
        with state_lock("build"):
            linux_esp.parent.mkdir(parents=True, exist_ok=True)
            run_checked([
                "bash", str(REPO_ROOT / "scripts/build-x86-uefi-esp.sh"), "--mode", "linux",
                "--kernel", str(linux_kernel), "--output", str(linux_esp),
                "--grub-config", str(REPO_ROOT / "config/x86_64/grub-linux-shell.cfg"),
            ])
    elif not linux_esp.is_file():
        raise ProductError(f"--no-build ABI differential requires existing Linux ESP: {linux_esp}")
    with state_lock("build", shared=True):
        validate_artifact_config(artifacts, rootfs, "drive")
        directory = run_abi_differential(AbiConfig(
            targets=(BenchmarkTarget("baseline", artifacts.kernel, artifacts.drive_esp),
                     BenchmarkTarget("linux", linux_kernel, linux_esp)),
            rootfs=rootfs, workdir=output,
            cpus=resolve_run_cpus(args.smp, args.run_cpus), memory=args.memory, timeout=args.timeout,
        ))
    print(f"ABI portable differential: {len(ABI_CONTRACTS)} contracts passed on both guests; logs={directory}")
    return 0


def bench_cmd(args: argparse.Namespace) -> int:
    if args.suite == "graphics":
        if not args.rootfs or not args.workdir or not args.linux_oracle_log:
            raise ProductError("graphics benchmark requires --rootfs, --workdir and --linux-oracle-log")
        return graphics_benchmark_cmd(args)
    if getattr(args, "m5_candidate", False):
        raise ProductError("benchmark baseline must use default policies; supply prepared candidate kernel and ESP paths")
    if args.accel != "kvm":
        raise ProductError("benchmark comparisons require --accel kvm")
    if not 32 <= args.iterations <= 1_000_000 or args.trials < 1:
        raise ProductError("benchmark requires 32..1000000 iterations and positive trials")
    if bool(args.candidate_kernel) != bool(args.candidate_esp):
        raise ProductError("candidate comparison requires both --candidate-kernel and --candidate-esp")
    artifacts = Artifacts(state_root(), parse_variant(args), "shell")
    rootfs = Path(args.rootfs).expanduser().resolve() if args.rootfs else artifacts.rootfs
    if not args.no_build:
        if not args.rootfs:
            build_rootfs(artifacts)
        build_kernel(artifacts, rootfs=rootfs, rootfs_transport="drive")
    linux_kernel = Path(args.linux_kernel).expanduser().resolve() if args.linux_kernel else None
    if linux_kernel is None:
        if args.no_build:
            raise ProductError("--no-build comparison requires --linux-kernel")
        completed = subprocess.run(
            ["bash", str(REPO_ROOT / "scripts/build-linux-oracle.sh"), "--jobs",
             os.environ.get("CARGO_BUILD_JOBS", "2")],
            cwd=REPO_ROOT, capture_output=False, stdout=subprocess.PIPE, text=True,
            check=False,
        )
        if completed.returncode:
            raise ProductError("Linux oracle build failed")
        linux_kernel = Path(completed.stdout.strip()).resolve()
    if not linux_kernel.is_file():
        raise ProductError(f"Linux oracle kernel is missing: {linux_kernel}")
    output = Path(args.workdir).expanduser().resolve() if args.workdir else state_root() / "runs"
    validate_storage(output)
    linux_esp = state_root() / "out/linux-7.2.3/benchmark.esp"
    if args.no_build:
        if not linux_esp.is_file():
            raise ProductError(f"--no-build comparison requires existing Linux ESP: {linux_esp}")
    else:
        with state_lock("build"):
            linux_esp.parent.mkdir(parents=True, exist_ok=True)
            run_checked([
                "bash", str(REPO_ROOT / "scripts/build-x86-uefi-esp.sh"), "--mode", "linux",
                "--kernel", str(linux_kernel), "--output", str(linux_esp),
                "--grub-config", str(REPO_ROOT / "config/x86_64/grub-linux-shell.cfg"),
            ])
    targets = [BenchmarkTarget("baseline", artifacts.kernel, artifacts.drive_esp),
               BenchmarkTarget("linux", linux_kernel, linux_esp)]
    if args.candidate_kernel:
        targets.append(BenchmarkTarget("candidate", Path(args.candidate_kernel).resolve(),
                                       Path(args.candidate_esp).resolve()))
    try:
        host_cpus = tuple(int(cpu) for cpu in args.host_cpus.split(",")) if args.host_cpus else ()
    except ValueError as error:
        raise ProductError("--host-cpus must be a comma-separated list of CPU numbers") from error
    with state_lock("build", shared=True):
        validate_artifact_config(artifacts, rootfs, "drive")
        result = run_benchmark_experiment(BenchmarkConfig(
            targets=tuple(targets), rootfs=rootfs, workdir=output, suite=args.suite,
            iterations=args.iterations, trials=args.trials,
            cpus=resolve_run_cpus(args.smp, args.run_cpus), memory=args.memory,
            host_cpus=host_cpus, timeout=args.timeout,
        ))
    result_path = Path(result["workdir"]) / "results.json"
    result_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(result_path)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="./tools/thekernel.py")
    sub = parser.add_subparsers(dest="command", required=True)

    verify = sub.add_parser("verify", help="run repository verification (daily, full, hardware)")
    verify.add_argument("--tier", choices=("daily", "full", "hardware"), default="daily")
    verify.set_defaults(func=verify_cmd)

    build = sub.add_parser("build", help="build the x86_64 q35/UEFI kernel and ESP")
    add_variant_arguments(build)
    build.add_argument(
        "--rootfs",
        help="stage this existing rootfs image as the Multiboot2 module instead of building one",
    )
    build.add_argument("--rootfs-transport", choices=("module", "drive"), default="module")
    build.set_defaults(func=build_cmd)

    lint = sub.add_parser("lint", help="run Clippy for the product kernel configuration")
    add_variant_arguments(lint)
    lint.set_defaults(func=lint_cmd)

    clean = sub.add_parser("clean", help="remove generated run, output, and cache directories")
    clean.set_defaults(func=clean_cmd)

    run_parser = sub.add_parser("run", help="build and boot the product image")
    add_run_arguments(run_parser)
    run_parser.set_defaults(func=run_cmd)

    gui_parser = sub.add_parser("run-gui", help="build and boot the interactive Weston desktop")
    add_run_arguments(gui_parser)
    gui_parser.set_defaults(func=run_gui_cmd, profile="system", interactive=True,
                            graphics_profile="interactive", rootfs_transport="drive")

    test = sub.add_parser("test", help="run a checked host or guest suite")
    add_graphics_smoke_arguments(test)
    test.add_argument("--suite", choices=("host", "guest", "abi", "graphics", "cpu", "all"), required=True)
    test.add_argument("--run-cpus", type=int)
    test.add_argument("--allow-skip", action="store_true")
    test.add_argument("--qemu-debug", help="QEMU -d categories; write workdir/qemu-debug.log")
    test.add_argument("--gdb", action="store_true",
                      help="graphics smoke: serve workdir/gdb.sock and pause on guest shutdown/reboot/panic")
    test.add_argument("--linux-kernel", help="already built Linux 7.2.3 oracle bzImage for ABI differential")
    test.set_defaults(func=test_cmd)
    bench = sub.add_parser("bench", help="run scheduler or I/O comparison experiments")
    add_graphics_benchmark_arguments(bench)
    bench.add_argument("--no-build", action="store_true")
    bench.add_argument("--run-cpus", type=int)
    bench.add_argument("--iterations", type=int, default=1000)
    bench.add_argument("--trials", type=int, default=10)
    bench.add_argument("--linux-kernel", help="already built Linux 7.2.3 oracle bzImage")
    bench.add_argument("--candidate-kernel")
    bench.add_argument("--candidate-esp")
    bench.add_argument("--host-cpus", help="common QEMU CPU affinity mask, e.g. 0,1,2,3")
    bench.add_argument("--suite", choices=("scheduler", "io", "graphics", "all"), required=True)
    bench.set_defaults(func=bench_cmd)
    return parser


def main(argv: list[str] | None = None) -> int:
    try:
        args = build_parser().parse_args(argv)
        with state_lock("activity", shared=args.command != "clean", blocking=args.command != "clean"):
            return int(args.func(args))
    except (ProductError, RunnerError, ProcessError, OSError) as error:
        print(f"thekernel: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
