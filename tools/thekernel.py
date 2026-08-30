#!/usr/bin/env python3
"""Direct product build, boot, and system-test entry point for TheKernel."""

from __future__ import annotations

import argparse
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
from tools.qemu_runner.model import QmpColorBlock, QmpControls


TARGET = "x86_64-unknown-none"
PLATFORM = "x86-pc"
COMPLETION_MARKER = "# THEKERNEL_SYSTEM_TEST_COMPLETE"
SYSTEM_TEST_SHUTDOWN_COMMANDS = "/bin/busybox poweroff -f\nexit\n"
MAX_KERNEL_BYTES = 800 * 1024 * 1024
KERNEL_LOAD_PADDR = 2 * 1024 * 1024
Q35_MMIO_BASE = 2 * 1024 * 1024 * 1024
MEMORY_RE = re.compile(r"([1-9][0-9]*)([KMG])", re.IGNORECASE)


class ProductError(RuntimeError):
    """Raised for an invalid or failed product operation."""


def state_root() -> Path:
    configured = os.environ.get("THEKERNEL_STATE_DIR", "").strip()
    state = Path(configured).expanduser() if configured else REPO_ROOT / ".state"
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
    def rootfs(self) -> Path:
        return self.root / "out" / "rootfs" / "x86" / "rootfs-x86.img"


def parse_variant(args: argparse.Namespace) -> Variant:
    memory = args.memory.upper()
    if not MEMORY_RE.fullmatch(memory):
        raise ProductError(f"--memory must be a positive K/M/G size: {args.memory}")
    variant = Variant(cpus=args.smp, memory=memory, asid_fast_switch=args.asid_fast_switch)
    if variant.memory_bytes <= KERNEL_LOAD_PADDR:
        raise ProductError("--memory must extend beyond the 2 MiB kernel load address")
    if variant.memory_bytes > Q35_MMIO_BASE:
        raise ProductError("--memory must not overlap the q35 MMIO window at 2 GiB")
    if args.smp < 1 or args.smp > 4096:
        raise ProductError(f"--smp must be an integer between 1 and 4096: {args.smp}")
    return variant


def positive_timeout(value: str) -> float:
    try:
        timeout = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("timeout must be a number") from error
    if timeout <= 0 or not math.isfinite(timeout):
        raise argparse.ArgumentTypeError("timeout must be a positive finite number")
    return timeout


def command_env(artifacts: Artifacts) -> dict[str, str]:
    linker_args = (
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
        "AX_LOG": "off",
        # QEMU user networking's fixed product subnet.  axnet-ng consumes
        # these at compile time and rejects an absent address at boot.
        "AX_IP": "10.0.2.15",
        "AX_GW": "10.0.2.2",
        "SMOLTCP_IFACE_MAX_ADDR_COUNT": "4",
        "AX_TARGET": TARGET,
        "AX_START_BANNER": "n",
        "AX_BACKTRACE": "n",
        "AX_CONFIG_PATH": str(artifacts.config_path),
        "AX_LINKER_SCRIPT_OUTPUT": str(artifacts.linker_script),
        "CARGO_TARGET_DIR": str(artifacts.cargo_target_dir),
        "RUSTFLAGS": " ".join(part for part in (inherited_rustflags, linker_args) if part),
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
    run_checked(
        [
            generator,
            str(REPO_ROOT / "config" / "kernel.toml"),
            str(REPO_ROOT / "config" / "x86_64" / "q35-uefi.toml"),
            "-w",
            'arch="x86_64"',
            "-w",
            f"plat.max-cpu-num={artifacts.variant.cpus}",
            "-w",
            f"plat.phys-memory-size={artifacts.variant.memory_bytes}",
            "-o",
            str(artifacts.config_path),
        ]
    )


def kernel_features(artifacts: Artifacts) -> str:
    variant = artifacts.variant
    features = ["qemu"]
    if artifacts.profile == "shell":
        features.append("boot-shell")
    if variant.cpus > 1:
        features.append("smp")
    if variant.asid_fast_switch:
        features.append("asid-fast-switch")
    return " ".join(features)


def build_kernel(artifacts: Artifacts) -> None:
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
    if KERNEL_LOAD_PADDR + artifacts.kernel.stat().st_size > artifacts.variant.memory_bytes:
        raise ProductError(
            f"kernel does not fit in {artifacts.variant.memory} of physical memory"
        )
    run_checked(["bash", str(REPO_ROOT / "scripts" / "check-x86-multiboot.sh"), str(artifacts.kernel)], env=env)
    run_checked(
        [
            "bash",
            str(REPO_ROOT / "scripts" / "build-x86-uefi-esp.sh"),
            "--kernel",
            str(artifacts.kernel),
            "--output",
            str(artifacts.esp),
            "--grub-config",
            str(REPO_ROOT / "config" / "x86_64" / "grub.cfg"),
        ],
        env=env,
    )


def build_rootfs(artifacts: Artifacts) -> None:
    artifacts.rootfs.parent.mkdir(parents=True, exist_ok=True)
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
            "axnet-ng",
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
    qmp_screenshot: Path | None = None,
    qmp_screenshot_after_marker: str | None = None,
    qmp_screenshot_size: tuple[int, int] | None = None,
    qmp_screenshot_color_blocks: tuple[QmpColorBlock, ...] = (),
) -> int:
    if not artifacts.kernel.is_file() or not artifacts.esp.is_file():
        raise ProductError("kernel and ESP are required; run `thekernel.py build` first")
    selected_rootfs = rootfs if rootfs is not None else artifacts.rootfs
    if not selected_rootfs.is_file():
        raise ProductError("rootfs is required; run `thekernel.py rootfs` first")
    if workdir is not None:
        run_dir = workdir.expanduser().resolve()
    else:
        runs_root = artifacts.root / "runs"
        runs_root.mkdir(parents=True, exist_ok=True)
        run_dir = Path(tempfile.mkdtemp(prefix=f"{artifacts.profile}-", dir=runs_root))
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
    if qmp_screenshot is not None:
        qmp = QmpControls(
            socket=run_dir / "graphics-smoke.qmp",
            screenshot=qmp_screenshot.expanduser().resolve(),
            screenshot_after_marker=qmp_screenshot_after_marker,
            screenshot_size=qmp_screenshot_size,
            screenshot_color_blocks=qmp_screenshot_color_blocks,
        )
    result = run(
        RunConfig(
            arch="x86_64",
            kernel=artifacts.kernel,
            rootfs=selected_rootfs,
            esp=artifacts.esp,
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
            cpus=artifacts.variant.cpus,
            accel=accel,
            graphics_profile=graphics_profile,
            qmp=qmp,
        ),
    )
    print(f"qemu-runner exit={result.returncode} log={result.log_path}", file=sys.stderr)
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
    build_kernel(artifacts)
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


def run_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), args.profile)
    rootfs = Path(args.rootfs).expanduser().resolve() if args.rootfs else None
    if rootfs is not None and not rootfs.is_file():
        raise ProductError(f"rootfs does not exist: {rootfs}")
    if not args.no_build:
        build_kernel(artifacts)
        if rootfs is None:
            build_rootfs(artifacts)
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
    )


def system_test_cmd(args: argparse.Namespace) -> int:
    artifacts = Artifacts(state_root(), parse_variant(args), "system")
    build_kernel(artifacts)
    build_rootfs(artifacts)
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
    )


def graphics_smoke_cmd(args: argparse.Namespace) -> int:
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
    if args.graphics_profile == "virgl-interactive" and args.flavor != "q35-software-desktop":
        raise ProductError("virgl-interactive graphics smoke requires --flavor q35-software-desktop")
    if not args.no_build:
        build_kernel(artifacts)
    marker = (
        "THEKERNEL_Q35_VIRGL_READY"
        if args.graphics_profile == "virgl-interactive"
        else "THEKERNEL_Q35_WESTON_READY"
        if args.flavor == "q35-software-desktop"
        else "THEKERNEL_GRAPHICS_ABI_SMOKE_READY"
    )
    blocks = (QmpColorBlock(300, 200, 200, 200, (255, 0, 0)),) if args.flavor == "q35-software-desktop" else ()
    size = (800, 600) if args.flavor == "q35-software-desktop" else None
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
        qmp_screenshot=screenshot,
        qmp_screenshot_after_marker=marker,
        qmp_screenshot_size=size,
        qmp_screenshot_color_blocks=blocks,
    )


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
        choices=("headless", "interactive", "virgl-headless", "virgl-interactive"),
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
    parser.add_argument("--flavor", choices=("headless-abi-smoke", "q35-software-desktop"), default="headless-abi-smoke")
    parser.add_argument("--screenshot", required=True, help="QMP screendump PPM output path")
    parser.add_argument("--no-build", action="store_true", help="reuse existing kernel and ESP artifacts")
    parser.add_argument("--accel", choices=("tcg", "kvm"), default="tcg")
    parser.add_argument("--timeout", type=positive_timeout, default=300.0)
    parser.add_argument("--workdir")
    parser.add_argument(
        "--graphics-profile",
        choices=("headless", "interactive", "virgl-interactive"),
        default="headless",
        help="QEMU display topology; headless uses QMP screendump without a host window",
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="./tools/thekernel.py")
    sub = parser.add_subparsers(dest="command", required=True)

    build = sub.add_parser("build", help="build the x86_64 q35/UEFI kernel and ESP")
    add_variant_arguments(build)
    build.set_defaults(func=build_cmd)

    rootfs = sub.add_parser("rootfs", help="build the x86_64 semantic root filesystem")
    rootfs.set_defaults(func=rootfs_cmd)

    lint = sub.add_parser("lint", help="run Clippy for the product kernel configuration")
    add_variant_arguments(lint)
    lint.set_defaults(func=lint_cmd)

    run_parser = sub.add_parser("run", help="build and boot the product image")
    add_run_arguments(run_parser)
    run_parser.set_defaults(func=run_cmd)

    graphics_smoke = sub.add_parser(
        "graphics-smoke",
        help="boot a graphics rootfs and capture a marker-gated QMP screenshot",
    )
    add_graphics_smoke_arguments(graphics_smoke)
    graphics_smoke.set_defaults(func=graphics_smoke_cmd)

    system_test = sub.add_parser("system-test", help="build and run the product system-test suite")
    add_variant_arguments(system_test, profiles=False)
    system_test.add_argument("--accel", choices=("tcg", "kvm"), default="tcg")
    system_test.add_argument("--timeout", type=positive_timeout, default=300.0)
    system_test.add_argument("--workdir")
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
