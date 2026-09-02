#!/usr/bin/env python3
"""Direct product build, boot, and system-test entry point for TheKernel."""

from __future__ import annotations

import argparse
import glob as glob_module
import hashlib
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from tools.qemu_runner import (
    Interaction,
    ProcessError,
    RunConfig,
    RunLimits,
    RunnerError,
    run,
)
from tools.qemu_runner.model import QmpCheckpoint, QmpColorBlock, QmpControls
from tools.qemu_runner.graphics_benchmark import (
    BENCHMARK_COMPLETE_MARKER,
    benchmark_checkpoints,
    renderer_for_profile,
)
from tools.qemu_runner.graphics_metrics import GraphicsMetricError, enforce_graphics_metrics, parse_graphics_metrics


TARGET = "x86_64-unknown-none"
PLATFORM = "x86-pc"
PRODUCT_FEATURE = "x86-product"
PRODUCT_MAX_CPUS = 4
COMPLETION_MARKER = "# THEKERNEL_SYSTEM_TEST_COMPLETE"
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
MEMORY_RE = re.compile(r"([1-9][0-9]*)([KMG])", re.IGNORECASE)


class ProductError(RuntimeError):
    """Raised for an invalid or failed product operation."""


def state_root() -> Path:
    configured = os.environ.get("THEKERNEL_STATE_DIR", "").strip()
    # Product artifacts are intentionally outside the checkout.  Apart from
    # keeping the tree clean, this keeps all large, regenerable targets on the
    # host filesystem rather than a transient mount.
    state = (
        Path(configured).expanduser()
        if configured
        else Path.home() / ".cache" / "thekernel-targets"
    )
    if not state.is_absolute():
        state = REPO_ROOT / state
    return state.resolve()


@dataclass(frozen=True)
class Variant:
    cpus: int
    memory: str
    asid_fast_switch: bool = False

    @property
    def memory_bytes(self) -> int:
        match = MEMORY_RE.fullmatch(self.memory)
        assert match is not None
        value = int(match.group(1))
        shift = {"K": 10, "M": 20, "G": 30}[match.group(2).upper()]
        return value << shift

    @property
    def name(self) -> str:
        suffix = "-asid-fast-switch" if self.asid_fast_switch else ""
        return f"smp{self.cpus}-mem{self.memory.lower()}{suffix}"


@dataclass(frozen=True)
class Artifacts:
    root: Path
    variant: Variant
    profile: str = "system"

    @property
    def output_dir(self) -> Path:
        return self.root / "out" / "x86_64" / "q35-uefi" / self.profile / self.variant.name

    @property
    def cargo_target_dir(self) -> Path:
        return self.root / "target" / "thekernel" / "x86_64" / "q35-uefi" / self.profile / self.variant.name

    @property
    def config_path(self) -> Path:
        return self.cargo_target_dir / "config" / "axconfig.toml"

    @property
    def linker_script(self) -> Path:
        return self.cargo_target_dir / TARGET / "release" / f"linker_{PLATFORM}.lds"

    @property
    def cargo_elf(self) -> Path:
        return self.cargo_target_dir / TARGET / "release" / "thekernel"

    @property
    def kernel(self) -> Path:
        return self.output_dir / "kernel-x86_64"

    @property
    def esp(self) -> Path:
        return self.output_dir / "kernel-x86_64.esp"

    @property
    def drive_esp(self) -> Path:
        """UEFI ESP for a rootfs supplied exclusively as virtio-blk."""

        return self.output_dir / "kernel-x86_64-drive.esp"

    def esp_for_rootfs_transport(self, rootfs_transport: str) -> Path:
        if rootfs_transport == "module":
            return self.esp
        if rootfs_transport == "drive":
            return self.drive_esp
        raise ProductError(f"unsupported product rootfs transport: {rootfs_transport}")

    @property
    def rootfs(self) -> Path:
        return self.root / "out" / "rootfs" / "x86" / "rootfs-x86.img"


def parse_variant(args: argparse.Namespace) -> Variant:
    memory = args.memory.upper()
    if not MEMORY_RE.fullmatch(memory):
        raise ProductError(f"--memory must be a positive K/M/G size: {args.memory}")
    variant = Variant(cpus=args.smp, memory=memory, asid_fast_switch=args.asid_fast_switch)
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


def resolve_run_cpus(artifacts: Artifacts, run_cpus: int | None) -> int:
    """Select the QEMU CPU count without changing the built artifact variant."""

    if run_cpus is None:
        return artifacts.variant.cpus
    if run_cpus < 1 or run_cpus > artifacts.variant.cpus:
        raise ProductError(
            "--run-cpus must be an integer between 1 and the artifact "
            f"--smp value ({artifacts.variant.cpus}): {run_cpus}"
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
    return {
        **os.environ,
        "AX_ARCH": "x86_64",
        "AX_PLATFORM": PLATFORM,
        "AX_MODE": "release",
        # Debugging aid: AX_LOG=debug ./tools/thekernel.py run ... rebuilds
        # with kernel logging instead of the silent product default.
        "AX_LOG": os.environ.get("AX_LOG") or "off",
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
    return " ".join(features)


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
    run_checked(
        [str(llvm_objcopy()), "--strip-all", str(artifacts.cargo_elf), str(artifacts.kernel)],
        env=env,
    )
    if not artifacts.kernel.is_file() or artifacts.kernel.stat().st_size == 0:
        raise ProductError(f"kernel output is empty: {artifacts.kernel}")
    if artifacts.kernel.stat().st_size > MAX_KERNEL_BYTES:
        raise ProductError(f"kernel exceeds {MAX_KERNEL_BYTES} bytes: {artifacts.kernel}")
    low_ram_bytes = min(artifacts.variant.memory_bytes, Q35_PCI_HOLE_LOW_RAM_LIMIT)
    if KERNEL_LOAD_PADDR + artifacts.kernel.stat().st_size > low_ram_bytes:
        raise ProductError(
            "kernel does not fit below the q35 PCI hole "
            f"({low_ram_bytes // (1024 * 1024)} MiB of low RAM)"
        )
    run_checked(["bash", str(REPO_ROOT / "scripts" / "check-x86-multiboot.sh"), str(artifacts.kernel)], env=env)
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


# Inputs that change the published rootfs image.  The BusyBox version and
# download URL live in build-rootfs.sh itself, so hashing the script covers
# them.
ROOTFS_INPUT_FILES = (
    "scripts/build-rootfs.sh",
    "scripts/create-rootfs-image.sh",
    "tests/guest/shell-init.sh",
    "tests/guest/system-init.c",
)
ROOTFS_INPUT_GLOBS = (
    "tests/rootfs/busybox-*.config",
    "tests/guest/tools/*.c",
    "tests/guest/portable/*.c",
)
# Environment switches that change the toolchain or image ownership.
ROOTFS_INPUT_ENV = (
    "THEKERNEL_X86_CROSS_COMPILE",
    "THEKERNEL_USE_LOCAL_MUSL",
    "THEKERNEL_MUSL_ROOT",
    "THEKERNEL_ROOTFS_OWNER_MODE",
)


def rootfs_stamp_path(artifacts: Artifacts) -> Path:
    return artifacts.rootfs.with_name(artifacts.rootfs.name + ".stamp")


def rootfs_fingerprint() -> str:
    digest = hashlib.sha256()
    inputs = [REPO_ROOT / relative for relative in ROOTFS_INPUT_FILES]
    for pattern in ROOTFS_INPUT_GLOBS:
        inputs.extend(
            Path(path) for path in sorted(glob_module.glob(str(REPO_ROOT / pattern)))
        )
    for path in inputs:
        digest.update(path.name.encode())
        digest.update(path.read_bytes())
    for name in ROOTFS_INPUT_ENV:
        digest.update(f"{name}={os.environ.get(name, '')}".encode())
    return digest.hexdigest()


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
    stamp.write_text(fingerprint + "\n", encoding="utf-8")


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
            "warnings",
            "-A",
            "dead-code",
            "-A",
            "clippy::drop-non-drop",
            "-A",
            "clippy::too-many-arguments",
        ],
        env=command_env(artifacts),
    )


def prune_run_dirs(runs_root: Path, keep: Path) -> None:
    # Auto-prune keeps only the run directory it just created; interactive
    # concurrent runs are not supported by this scheme.
    for entry in runs_root.iterdir():
        if entry == keep or not entry.is_dir():
            continue
        shutil.rmtree(entry, ignore_errors=True)


def run_product(
    artifacts: Artifacts,
    *,
    accel: str,
    timeout: float,
    workdir: Path | None,
    interactive: bool,
    input_after_marker: str | None,
    stop_after_marker: str | None,
    commands: Path | None,
    extra_block: Path | None,
    shutdown_after_marker: bool = False,
    reject_ktap_skips: bool = False,
    graphics_profile: str = "headless",
    rootfs: Path | None = None,
    rootfs_transport: str = "module",
    qmp_screenshot: Path | None = None,
    qmp_screenshot_after_marker: str | None = None,
    qmp_screenshot_size: tuple[int, int] | None = None,
    qmp_screenshot_color_blocks: tuple[QmpColorBlock, ...] = (),
    qmp_checkpoints: tuple[QmpCheckpoint, ...] = (),
    qmp_timeout_secs: float = 5.0,
    graphics_width: int = 800,
    graphics_height: int = 600,
    run_cpus: int | None = None,
) -> int:
    try:
        selected_esp = artifacts.esp_for_rootfs_transport(rootfs_transport)
    except ProductError:
        raise ProductError("the x86 product supports only module or drive rootfs transport") from None
    if not artifacts.kernel.is_file() or not selected_esp.is_file():
        raise ProductError("kernel and ESP are required; run `thekernel.py build` first")
    selected_rootfs = rootfs if rootfs is not None else artifacts.rootfs
    if not selected_rootfs.is_file():
        raise ProductError("rootfs is required; run `thekernel.py rootfs` first")
    qemu_cpus = resolve_run_cpus(artifacts, run_cpus)
    if workdir is not None:
        run_dir = workdir.expanduser().resolve()
    else:
        runs_root = artifacts.root / "runs"
        runs_root.mkdir(parents=True, exist_ok=True)
        run_dir = Path(tempfile.mkdtemp(prefix=f"{artifacts.profile}-", dir=runs_root))
        prune_run_dirs(runs_root, run_dir)
    try:
        run_dir.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise ProductError(f"cannot create run directory: {error}") from error
    command_path = None
    if shutdown_after_marker:
        if commands is not None or input_after_marker is not None or stop_after_marker is not None:
            raise ProductError("system shutdown-after-marker cannot combine custom marker or command input")
        command_path = run_dir / "system-test-shutdown.commands"
        try:
            command_path.write_text(SYSTEM_TEST_SHUTDOWN_COMMANDS, encoding="utf-8")
        except OSError as error:
            raise ProductError(f"cannot write system-test shutdown command: {error}") from error
        input_after_marker = COMPLETION_MARKER
        interactive = True
    if commands is not None:
        command_path = commands.expanduser().resolve()
        if not command_path.is_file():
            raise ProductError(f"commands file does not exist: {command_path}")
    qmp = QmpControls()
    if qmp_screenshot is not None or qmp_checkpoints:
        qmp = QmpControls(
            socket=run_dir / "graphics-smoke.qmp",
            screenshot=(None if qmp_checkpoints else qmp_screenshot.expanduser().resolve()),
            screenshot_after_marker=(None if qmp_checkpoints else qmp_screenshot_after_marker),
            screenshot_size=(None if qmp_checkpoints else qmp_screenshot_size),
            screenshot_color_blocks=( () if qmp_checkpoints else qmp_screenshot_color_blocks),
            checkpoints=qmp_checkpoints,
            timeout_secs=qmp_timeout_secs,
        )
    result = run(
        RunConfig(
            arch="x86_64",
            kernel=artifacts.kernel,
            rootfs=selected_rootfs,
            rootfs_transport=rootfs_transport,
            esp=selected_esp,
            extra_block=extra_block.expanduser().resolve() if extra_block else None,
            input_path=command_path,
            workdir=run_dir,
            log_path=run_dir / "console.log",
            limits=RunLimits(total_timeout_secs=timeout),
            interaction=Interaction(
                interactive=interactive or command_path is not None,
                input_after_marker=input_after_marker,
                stop_after_marker=stop_after_marker,
            ),
            memory=artifacts.variant.memory,
            cpus=qemu_cpus,
            accel=accel,
            graphics_profile=graphics_profile,
            graphics_width=graphics_width,
            graphics_height=graphics_height,
            qmp=qmp,
        ),
    )
    print(f"qemu-runner exit={result.returncode} log={result.log_path}", file=sys.stderr)
    if result.error_message is not None:
        print(f"qemu-runner error={result.error_message}", file=sys.stderr)
    if stop_after_marker is not None:
        if result.intentionally_stopped:
            return 0
        print(
            f"thekernel: guest exited without completion marker: {stop_after_marker}",
            file=sys.stderr,
        )
        return result.returncode if result.returncode != 0 else 1
    if result.guest_clean_shutdown and reject_ktap_skips:
        reject_ktap_skips_in_log(result.log_path)
    return result.returncode


def reject_ktap_skips_in_log(log_path: Path) -> None:
    """Keep a green system-test run distinct from an unsupported test case."""

    try:
        text = log_path.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        raise ProductError(f"cannot read system-test log: {error}") from error
    skipped = [
        line
        for line in text.splitlines()
        if re.match(r"^ok\s+[1-9][0-9]*\b.*\s#\s*SKIP(?:\s|$)", line)
    ]
    if skipped:
        raise ProductError(
            f"system test contains {len(skipped)} KTAP SKIP result(s); pass --allow-skip to inspect a non-gating preview run"
        )


def build_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), args.profile)
    rootfs = Path(args.rootfs).expanduser().resolve() if args.rootfs else None
    if rootfs is None:
        build_rootfs(artifacts)
    elif not rootfs.is_file():
        raise ProductError(f"rootfs does not exist: {rootfs}")
    build_kernel(artifacts, rootfs=rootfs)
    print(artifacts.kernel)
    print(artifacts.esp)
    return 0


def rootfs_cmd(_args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), Variant(cpus=1, memory="128M"), "system")
    build_rootfs(artifacts)
    print(artifacts.rootfs)
    return 0


def lint_cmd(args: argparse.Namespace) -> int:
    lint_kernel(Artifacts(state_root(), parse_variant(args), args.profile))
    return 0


# Known generated subdirectories below the state root.  The state root itself
# and anything else under it are user data and are never removed.
CLEAN_STATE_DIRS = ("runs", "out", "target/thekernel", "source-cache")


def clean_cmd(_args: argparse.Namespace) -> int:
    removed = False
    root = state_root()
    for name in CLEAN_STATE_DIRS:
        path = root / name
        if path.is_dir():
            shutil.rmtree(path, ignore_errors=True)
            print(path)
            removed = True
    tmp_root = REPO_ROOT / ".tmp"
    if tmp_root.is_dir():
        for entry in tmp_root.glob("rootfs.*"):
            if entry.is_dir():
                shutil.rmtree(entry, ignore_errors=True)
                print(entry)
                removed = True
    if not removed:
        print("thekernel: nothing to clean")
    return 0


def run_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), args.profile)
    run_cpus = resolve_run_cpus(artifacts, args.run_cpus)
    rootfs = Path(args.rootfs).expanduser().resolve() if args.rootfs else None
    if rootfs is not None and not rootfs.is_file():
        raise ProductError(f"rootfs does not exist: {rootfs}")
    if not args.no_build:
        if rootfs is None:
            build_rootfs(artifacts)
        build_kernel(artifacts, rootfs=rootfs)
    input_after_marker = args.input_after_marker
    if (
        args.commands
        and input_after_marker is None
        and artifacts.profile == "shell"
    ):
        input_after_marker = "THEKERNEL_SHELL_READY"
    return run_product(
        artifacts,
        accel=args.accel,
        timeout=args.timeout,
        workdir=Path(args.workdir) if args.workdir else None,
        interactive=args.interactive,
        graphics_profile=args.graphics_profile,
        input_after_marker=input_after_marker,
        stop_after_marker=args.stop_after_marker,
        commands=Path(args.commands) if args.commands else None,
        extra_block=Path(args.extra_block) if args.extra_block else None,
        rootfs=rootfs,
        rootfs_transport="module",
        run_cpus=run_cpus,
    )


def system_test_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), "system")
    run_cpus = resolve_run_cpus(artifacts, args.run_cpus)
    if not args.no_build:
        build_rootfs(artifacts)
        build_kernel(artifacts)
    return run_product(
        artifacts,
        accel=args.accel,
        timeout=args.timeout,
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
    seatd_flavors = {"q35-graphics-seatd", "q35-software-desktop", "q35-venus-desktop"}
    if args.graphics_profile == "virgl-interactive" and args.flavor not in (seatd_flavors | {"q35-graphics-logind"}):
        raise ProductError("virgl-interactive graphics smoke requires --flavor q35-graphics-seatd or q35-graphics-logind")
    if args.graphics_profile == "venus-interactive" and args.flavor not in (seatd_flavors | {"q35-graphics-logind"}):
        raise ProductError("venus-interactive graphics smoke requires --flavor q35-graphics-seatd or q35-graphics-logind")
    if not args.no_build:
        build_kernel(artifacts, rootfs=rootfs, rootfs_transport="drive")
    marker = (
        "THEKERNEL_Q35_SWAY_READY"
        if args.flavor == "q35-graphics-logind"
        else "THEKERNEL_Q35_VIRGL_READY"
        if args.graphics_profile == "virgl-interactive"
        else "THEKERNEL_Q35_VENUS_READY"
        if args.graphics_profile == "venus-interactive"
        else "THEKERNEL_Q35_WESTON_READY"
        if args.flavor in {"q35-graphics-seatd", "q35-software-desktop"}
        else "THEKERNEL_GRAPHICS_ABI_SMOKE_READY"
    )
    blocks = (QmpColorBlock(300, 200, 200, 200, (255, 0, 0)),) if args.flavor in {"q35-graphics-seatd", "q35-software-desktop"} else ()
    size = (800, 600) if args.flavor in {"q35-graphics-seatd", "q35-software-desktop"} else None
    checkpoints: tuple[QmpCheckpoint, ...] = ()
    if args.flavor in {"q35-graphics-seatd", "q35-software-desktop"} and args.graphics_profile == "virgl-interactive":
        # The rootless Xwayland client publishes this marker only after its
        # mapped, drawn, resized, focused, clipboard-owning foreground window
        # is stacked above the background window.  Inject directly into the
        # virtio devices while it records the resulting X11 key/pointer events.
        checkpoints = (
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
    elif args.flavor == "q35-graphics-logind":
        # The guest does not enter its A -> B -> A logind handoff until this
        # initial pointer checkpoint has reached Alice's persistent Wayland
        # client.  The final checkpoint observes the same session restored
        # after all revocation/ACL transitions.
        logind_checkpoints: list[QmpCheckpoint] = [
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
            logind_checkpoints.append(QmpCheckpoint(
                input_after_marker=f"THEKERNEL_Q35_LOGIND_CYCLE_BOB_POINTER_READY_{cycle:03d}",
                input_events=((
                    {"type": "rel", "data": {"axis": "x", "value": 1}},
                ),),
            ))
            logind_checkpoints.append(QmpCheckpoint(
                input_after_marker=f"THEKERNEL_Q35_LOGIND_CYCLE_ALICE_KEY_READY_{cycle:03d}",
                input_events=((
                    {"type": "key", "data": {"down": True, "key": {"type": "qcode", "data": "a"}}},
                    {"type": "key", "data": {"down": False, "key": {"type": "qcode", "data": "a"}}},
                ),),
            ))
        logind_checkpoints.append(
            QmpCheckpoint(
                input_after_marker="THEKERNEL_Q35_LOGIND_CYCLES_COMPLETE",
                screenshot=screenshot.with_name(f"{screenshot.stem}-logind-cycles{''.join(screenshot.suffixes)}"),
                screenshot_after_marker="THEKERNEL_Q35_LOGIND_CYCLES_COMPLETE",
                screenshot_size=(800, 600),
                screenshot_color_blocks=(QmpColorBlock(300, 200, 200, 200, (0, 102, 255)),),
            )
        )
        checkpoints = tuple(logind_checkpoints)
    elif args.flavor in {"q35-graphics-seatd", "q35-software-desktop"}:
        # QMP drives the virtio keyboard/tablet devices directly.  Every
        # action waits for the preceding client repaint marker before taking
        # its own screenshot, so ordering is observable rather than timing
        # dependent.  The original screenshot remains the initial red SHM
        # frame; these files carry the later input checkpoints.
        checkpoints = (
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
    return run_product(
        artifacts,
        accel=args.accel,
        timeout=args.timeout,
        workdir=Path(args.workdir) if args.workdir else None,
        interactive=False,
        graphics_profile=args.graphics_profile,
        input_after_marker=None,
        stop_after_marker=marker,
        commands=None,
        extra_block=None,
        rootfs=rootfs,
        rootfs_transport="drive",
        qmp_screenshot=screenshot,
        qmp_screenshot_after_marker=marker,
        qmp_screenshot_size=size,
        qmp_screenshot_color_blocks=blocks,
        qmp_checkpoints=checkpoints,
        qmp_timeout_secs=900.0 if args.flavor == "q35-graphics-logind" else 120.0,
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
    build_kernel(artifacts, rootfs=rootfs, rootfs_transport="drive")
    run_dir = Path(args.workdir).expanduser().resolve() if args.workdir else None
    result = run_product(
        artifacts,
        accel="kvm",
        timeout=args.timeout,
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
    )
    if result:
        return result
    log = (run_dir / "console.log") if run_dir else None
    if log is None:
        raise ProductError("graphics benchmark requires --workdir to retain its metric log")
    try:
        current = parse_graphics_metrics(log)
        oracle = parse_graphics_metrics(Path(args.linux_oracle_log))
        expected_renderer = renderer_for_profile(args.graphics_profile)
        enforce_graphics_metrics(oracle, expected_renderer=expected_renderer)
        enforce_graphics_metrics(
            current,
            oracle,
            expected_renderer=expected_renderer,
        )
    except GraphicsMetricError as error:
        raise ProductError(str(error)) from error
    (run_dir / "graphics-metrics.json").write_text(current.json() + "\n", encoding="utf-8")
    print(current.json())
    return 0


def add_variant_arguments(parser: argparse.ArgumentParser, *, profiles: bool = True) -> None:
    parser.add_argument("--machine", choices=("q35",), default="q35")
    parser.add_argument("--firmware", choices=("uefi",), default="uefi")
    parser.add_argument("--smp", type=int, default=4)
    parser.add_argument("--memory", default="1G")
    parser.add_argument("--asid-fast-switch", action="store_true")
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
    parser.add_argument("--interactive", action="store_true")
    parser.add_argument(
        "--graphics-profile",
        choices=("headless", "interactive", "virgl-headless", "virgl-interactive", "venus-interactive"),
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


def add_graphics_smoke_arguments(parser: argparse.ArgumentParser) -> None:
    add_variant_arguments(parser)
    parser.add_argument("--rootfs", required=True, help="existing graphics rootfs.ext2 image")
    parser.add_argument("--flavor", choices=("headless-abi-smoke", "q35-graphics-seatd", "q35-software-desktop", "q35-venus-desktop", "q35-graphics-logind"), default="headless-abi-smoke")
    parser.add_argument("--screenshot", required=True, help="QMP screendump PPM output path")
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
        choices=("headless", "interactive", "virgl-interactive", "venus-interactive"),
        default="headless",
        help="QEMU display topology; headless uses QMP screendump without a host window",
    )


def add_graphics_benchmark_arguments(parser: argparse.ArgumentParser) -> None:
    add_variant_arguments(parser)
    parser.add_argument("--rootfs", required=True, help="q35-graphics-benchmark rootfs.ext2")
    parser.add_argument("--accel", choices=("kvm",), default="kvm")
    parser.add_argument("--timeout", type=positive_timeout, default=1800.0)
    parser.add_argument("--fault", choices=("modeset", "client-crash", "vt-switch", "weston-restart", "input-hotplug"))
    parser.add_argument("--workdir", required=True)
    parser.add_argument("--linux-oracle-log", required=True)
    parser.add_argument(
        "--graphics-profile",
        choices=("headless", "virgl-headless", "virgl-interactive", "venus-interactive"),
        required=True,
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="./tools/thekernel.py")
    sub = parser.add_subparsers(dest="command", required=True)

    build = sub.add_parser("build", help="build the x86_64 q35/UEFI kernel and ESP")
    add_variant_arguments(build)
    build.add_argument(
        "--rootfs",
        help="stage this existing rootfs image as the Multiboot2 module instead of building one",
    )
    build.set_defaults(func=build_cmd)

    rootfs = sub.add_parser("rootfs", help="build the x86_64 semantic root filesystem")
    rootfs.set_defaults(func=rootfs_cmd)

    lint = sub.add_parser("lint", help="run Clippy for the product kernel configuration")
    add_variant_arguments(lint)
    lint.set_defaults(func=lint_cmd)

    clean = sub.add_parser("clean", help="remove generated run, output, and cache directories")
    clean.set_defaults(func=clean_cmd)

    run_parser = sub.add_parser("run", help="build and boot the product image")
    add_run_arguments(run_parser)
    run_parser.set_defaults(func=run_cmd)

    graphics_smoke = sub.add_parser(
        "graphics-smoke",
        help="boot a graphics rootfs and capture a marker-gated QMP screenshot",
    )
    add_graphics_smoke_arguments(graphics_smoke)
    graphics_smoke.set_defaults(func=graphics_smoke_cmd)

    graphics_benchmark = sub.add_parser(
        "graphics-benchmark",
        help="run and enforce the short 60 warmup / 600 frame graphics gate",
    )
    add_graphics_benchmark_arguments(graphics_benchmark)
    graphics_benchmark.set_defaults(func=graphics_benchmark_cmd)

    system_test = sub.add_parser("system-test", help="build and run the product system-test suite")
    add_variant_arguments(system_test, profiles=False)
    system_test.add_argument("--accel", choices=("tcg", "kvm"), default="tcg")
    system_test.add_argument("--timeout", type=positive_timeout, default=300.0)
    system_test.add_argument("--workdir")
    system_test.add_argument(
        "--run-cpus",
        type=int,
        help="boot the --smp artifact with this many QEMU CPUs (1 through --smp)",
    )
    system_test.add_argument(
        "--no-build",
        action="store_true",
        help="run existing kernel, ESP, and rootfs artifacts without rebuilding",
    )
    system_test.add_argument(
        "--allow-skip",
        action="store_true",
        help="allow KTAP SKIP results for a non-gating preview run",
    )
    system_test.set_defaults(func=system_test_cmd)
    return parser


def main(argv: list[str] | None = None) -> int:
    try:
        args = build_parser().parse_args(argv)
        return int(args.func(args))
    except (ProductError, RunnerError, ProcessError) as error:
        print(f"thekernel: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
