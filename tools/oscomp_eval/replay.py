"""Direct OSComp replay and shell runner.

This module is the single QEMU entrypoint for local replay. It owns image
discovery, compressed-image caching, QEMU command construction, console logging,
judge execution, and scoring.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import lzma
import os
import select
import shutil
import signal
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from dataclasses import dataclass
from dataclasses import replace as dataclass_replace
from pathlib import Path
from typing import Any, Literal, TextIO

from .config import (
    JUDGE_TIMEOUT_SECS,
    Libc,
    REPLAY_TIMEOUT_FOCUSED_SECS,
    REPLAY_TIMEOUT_FULL_SECS,
    SHELL_TIMEOUT_SECS,
    effective_group_libc_matrix,
    expand_arches,
    group_libc_matrix_from_plan,
)
from .judge_runner import JudgeRunnerError, judge_log
from .markers import MarkerError
from .paths import create_run_dir, prepare_run_dir, repo_root
from .schemas import JudgeSummary, ScoreSummary
from .scoring import score_judge_summaries, write_score_summary
from .support_image import SupportImageBuild, build_support_image


Arch = Literal["rv", "la"]
Mode = Literal["replay", "shell"]


QEMU_MEMORY = "1G"
QEMU_SMP = "1"


class ReplayError(RuntimeError):
    """Raised for invalid replay setup."""


@dataclass(frozen=True)
class PreparedImage:
    source: Path
    runtime: Path
    cached: bool = False

    def to_json_dict(self) -> dict[str, object]:
        return {
            "source": str(self.source),
            "runtime": str(self.runtime),
            "cached": self.cached,
        }


@dataclass(frozen=True)
class ReplayResult:
    arch: str
    command: tuple[str, ...]
    returncode: int
    duration_ms: int
    log_path: Path
    workdir: Path
    error_message: str | None = None

    @property
    def ok(self) -> bool:
        return self.returncode == 0

    @property
    def timed_out(self) -> bool:
        return self.returncode == 124

    @property
    def interrupted(self) -> bool:
        return self.returncode in (130, -2)

    @property
    def launch_failed(self) -> bool:
        return (
            self.error_message is not None
            and self.error_message.startswith("replay launch failed:")
        )

    def to_json_dict(self, *, base_dir: Path | None = None) -> dict[str, object]:
        data: dict[str, object] = {
            "arch": self.arch,
            "command": list(self.command),
            "returncode": self.returncode,
            "duration_ms": self.duration_ms,
            "log_path": str(self.log_path),
            "workdir": str(self.workdir),
            "ok": self.ok,
            "timed_out": self.timed_out,
            "interrupted": self.interrupted,
            "launch_failed": self.launch_failed,
        }
        if self.error_message is not None:
            data["error"] = self.error_message
        if base_dir is not None:
            for field, path in (("log_relpath", self.log_path), ("workdir_relpath", self.workdir)):
                try:
                    data[field] = str(path.relative_to(base_dir))
                except ValueError:
                    pass
        return data


@dataclass(frozen=True)
class ReplayRunResult:
    run_dir: Path
    replays: tuple[ReplayResult, ...]
    judge_summaries: tuple[JudgeSummary, ...]
    score: ScoreSummary
    status: str
    support_image_build: SupportImageBuild | None = None

    @property
    def replay_failures(self) -> int:
        return sum(1 for replay in self.replays if not replay.ok)

    @property
    def timed_out(self) -> bool:
        return any(replay.timed_out for replay in self.replays)

    @property
    def interrupted(self) -> bool:
        return any(replay.interrupted for replay in self.replays)


def truthy(value: str | None) -> bool:
    return value in {"1", "y", "Y", "yes", "YES", "true", "TRUE", "on", "ON"}


def log_verbose(message: str, *, verbose: bool) -> None:
    if verbose:
        print(f"[oscomp-replay] {message}", file=sys.stderr)


def state_root(root: Path) -> Path:
    return Path(os.environ.get("OSCOMP_STATE_DIR", root / ".state"))


def workdir_base(root: Path) -> Path:
    return Path(os.environ.get("OSCOMP_WORKDIR_BASE", state_root(root) / "oscomp-replay"))


def image_cache_dir(root: Path) -> Path:
    return Path(os.environ.get("OSCOMP_IMAGE_CACHE_DIR", state_root(root) / "oscomp-image-cache"))


def testsuite_roots(root: Path) -> tuple[Path, ...]:
    roots: list[Path] = []

    def add(value: str | Path | None) -> None:
        if value is None or str(value) == "":
            return
        candidate = Path(value).expanduser()
        if not candidate.is_dir():
            return
        resolved = candidate.resolve()
        if resolved not in roots:
            roots.append(resolved)

    add(os.environ.get("OSCOMP_TESTSUITE_DIR"))
    add("/home/dia/kernel-image")
    add(Path.home() / "kernel-image")
    add(Path.home() / "testsuits-for-oskernel")
    add("/coursegrader/testdata")
    return tuple(roots)


def official_image_name(arch: Arch) -> str:
    return "sdcard-rv.img" if arch == "rv" else "sdcard-la.img"


def find_official_image(arch: Arch, *, root: Path) -> Path:
    base = official_image_name(arch)
    for directory in testsuite_roots(root):
        for name in (base, f"{base}.xz", f"{base}.gz"):
            candidate = directory / name
            if candidate.is_file():
                return candidate
    searched = ", ".join(str(path) for path in testsuite_roots(root)) or "<none>"
    raise ReplayError(f"official image for {arch} not found; searched: {searched}")


def first_existing(paths: tuple[Path, ...]) -> Path | None:
    for path in paths:
        if path.is_file():
            return path
    return None


def find_support_image(arch: Arch, *, root: Path) -> Path | None:
    if arch == "la":
        return first_existing(
            (
                root / "disk-la.img",
                root / "disk-la.img.xz",
                root / "disk.img",
                root / "disk.img.xz",
            )
        )
    return first_existing(
        (
            root / "disk.img",
            root / "disk.img.xz",
        )
    )


def cache_key(source: Path) -> str:
    stat = source.stat()
    resolved = source.resolve()
    digest = hashlib.sha256()
    digest.update(str(resolved).encode())
    digest.update(b"\0")
    digest.update(str(stat.st_size).encode())
    digest.update(b"\0")
    digest.update(str(stat.st_mtime_ns).encode())
    return digest.hexdigest()


def stale_lock(lock_dir: Path) -> bool:
    try:
        age = time.time() - lock_dir.stat().st_mtime
    except OSError:
        return False
    return age > 30 * 60


def decompress(source: Path, target: Path) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_name(f".{target.name}.tmp.{os.getpid()}")
    try:
        if source.name.endswith(".xz"):
            with lzma.open(source, "rb") as src, tmp.open("wb") as dst:
                shutil.copyfileobj(src, dst, length=1024 * 1024)
        elif source.name.endswith(".gz"):
            with gzip.open(source, "rb") as src, tmp.open("wb") as dst:
                shutil.copyfileobj(src, dst, length=1024 * 1024)
        else:
            raise ReplayError(f"unsupported compressed image: {source}")
        tmp.replace(target)
    except Exception:
        tmp.unlink(missing_ok=True)
        raise


def prepare_image(source: Path, *, root: Path, verbose: bool = False) -> PreparedImage:
    source = source.expanduser().resolve()
    if not source.is_file():
        raise ReplayError(f"image does not exist: {source}")
    if not (source.name.endswith(".xz") or source.name.endswith(".gz")):
        return PreparedImage(source=source, runtime=source, cached=False)

    name = source.name.removesuffix(".xz").removesuffix(".gz")
    key = cache_key(source)
    cache_dir = image_cache_dir(root) / key
    target = cache_dir / name
    lock_dir = cache_dir.with_suffix(".lock")
    cache_dir.parent.mkdir(parents=True, exist_ok=True)
    if target.is_file():
        log_verbose(f"using cached image: {target}", verbose=verbose)
        return PreparedImage(source=source, runtime=target, cached=True)

    while True:
        try:
            lock_dir.mkdir()
            break
        except FileExistsError:
            if target.is_file():
                log_verbose(f"using cached image: {target}", verbose=verbose)
                return PreparedImage(source=source, runtime=target, cached=True)
            if stale_lock(lock_dir):
                try:
                    lock_dir.rmdir()
                except OSError:
                    pass
            time.sleep(1)

    try:
        if target.is_file():
            return PreparedImage(source=source, runtime=target, cached=True)
        log_verbose(f"decompressing {source.name} into cache", verbose=verbose)
        decompress(source, target)
        return PreparedImage(source=source, runtime=target, cached=True)
    finally:
        try:
            lock_dir.rmdir()
        except OSError:
            pass


def drive_opts(path: Path, drive_id: str, *, mode: Literal["snapshot", "readonly", "rw"]) -> str:
    opts = f"file={path},if=none,format=raw,id={drive_id}"
    if mode == "snapshot":
        return f"{opts},snapshot=on"
    if mode == "readonly":
        return f"{opts},readonly=on"
    return opts


def qemu_binary(arch: Arch) -> str:
    return "qemu-system-riscv64" if arch == "rv" else "qemu-system-loongarch64"


def kernel_name(arch: Arch) -> str:
    return "kernel-rv" if arch == "rv" else "kernel-la"


def build_qemu_command(
    *,
    arch: Arch,
    kernel: Path,
    image: Path,
    support_image: Path | None,
    extra_block_image: Path | None = None,
) -> tuple[str, ...]:
    if arch == "rv":
        command = [
            "qemu-system-riscv64",
            "-machine",
            "virt",
            "-kernel",
            str(kernel),
            "-m",
            QEMU_MEMORY,
            "-nographic",
            "-smp",
            QEMU_SMP,
            "-bios",
            "default",
            "-drive",
            drive_opts(image, "x0", mode="snapshot"),
            "-device",
            "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0",
            "-no-reboot",
            "-device",
            "virtio-net-device,netdev=net",
            "-netdev",
            "user,id=net",
            "-rtc",
            "base=utc",
        ]
        if support_image is not None:
            command.extend(
                [
                    "-drive",
                    drive_opts(support_image, "x1", mode="readonly"),
                    "-device",
                    "virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1",
                ]
            )
        if extra_block_image is not None:
            command.extend(
                [
                    "-drive",
                    drive_opts(extra_block_image, "x2", mode="rw"),
                    "-device",
                    "virtio-blk-device,drive=x2,bus=virtio-mmio-bus.2",
                ]
            )
        return tuple(command)

    command = [
        "qemu-system-loongarch64",
        "-kernel",
        str(kernel),
        "-m",
        QEMU_MEMORY,
        "-nographic",
        "-smp",
        QEMU_SMP,
        "-drive",
        drive_opts(image, "x0", mode="snapshot"),
        "-device",
        "virtio-blk-pci,drive=x0",
        "-no-reboot",
        "-device",
        "virtio-net-pci,netdev=net0",
        "-netdev",
        "user,id=net0",
        "-rtc",
        "base=utc",
    ]
    if support_image is not None:
        command.extend(
            [
                "-drive",
                drive_opts(support_image, "x1", mode="readonly"),
                "-device",
                "virtio-blk-pci,drive=x1",
            ]
        )
    if extra_block_image is not None:
        command.extend(
            [
                "-drive",
                drive_opts(extra_block_image, "x2", mode="rw"),
                "-device",
                "virtio-blk-pci,drive=x2",
            ]
        )
    return tuple(command)


def append_log(log_path: Path, message: str) -> None:
    try:
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("a", encoding="utf-8") as log_file:
            log_file.write(f"[oscomp-replay] {message}\n")
    except OSError:
        pass


def terminate_process_group(process: subprocess.Popen[str], sig: int) -> None:
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass
    except OSError:
        process.terminate()


def wait_for_process(
    process: subprocess.Popen[str],
    *,
    log_file: TextIO,
    log_path: Path,
    timeout_secs: int | None,
    idle_timeout_secs: int | None,
    interactive: bool,
) -> tuple[int, str | None]:
    start = time.monotonic()
    last_output_at = start
    assert process.stdout is not None
    while True:
        ready, _, _ = select.select([process.stdout], [], [], 0.1)
        if ready:
            line = process.stdout.readline()
            if line:
                log_file.write(line)
                log_file.flush()
                if interactive:
                    print(line, end="")
                last_output_at = time.monotonic()

        returncode = process.poll()
        if returncode is not None:
            remainder = process.stdout.read()
            if remainder:
                log_file.write(remainder)
                log_file.flush()
                if interactive:
                    print(remainder, end="")
            return returncode, None

        now = time.monotonic()
        if timeout_secs is not None and timeout_secs > 0 and now - start >= timeout_secs:
            message = f"QEMU timed out after {timeout_secs}s"
            append_log(log_path, message)
            terminate_process_group(process, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                terminate_process_group(process, signal.SIGKILL)
                process.wait()
            return 124, message
        if (
            idle_timeout_secs is not None
            and idle_timeout_secs > 0
            and now - last_output_at >= idle_timeout_secs
        ):
            message = f"replay idle timeout after {idle_timeout_secs}s without console output"
            append_log(log_path, message)
            terminate_process_group(process, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                terminate_process_group(process, signal.SIGKILL)
                process.wait()
            return 124, message

        time.sleep(0.01)


def run_qemu(
    *,
    arch: Arch,
    command: tuple[str, ...],
    log_path: Path,
    workdir: Path,
    timeout_secs: int | None,
    idle_timeout_secs: int | None,
    interactive: bool = False,
) -> ReplayResult:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    start = time.monotonic()
    error_message: str | None = None
    try:
        stdin = None if interactive else subprocess.DEVNULL
        with log_path.open("w", encoding="utf-8", errors="ignore", buffering=1) as log_file:
            log_file.write(" ".join(command) + "\n")
            process = subprocess.Popen(
                command,
                cwd=workdir,
                stdin=stdin,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
                start_new_session=True,
            )
            returncode, error_message = wait_for_process(
                process,
                log_file=log_file,
                log_path=log_path,
                timeout_secs=timeout_secs,
                idle_timeout_secs=idle_timeout_secs,
                interactive=interactive,
            )
    except KeyboardInterrupt:
        try:
            terminate_process_group(process, signal.SIGTERM)  # type: ignore[has-type]
        except UnboundLocalError:
            pass
        returncode = 130
        error_message = "interrupted"
    except OSError as error:
        returncode = 3
        error_message = f"replay launch failed: {error}"
        append_log(log_path, error_message)

    duration_ms = int((time.monotonic() - start) * 1000)
    return ReplayResult(
        arch=arch,
        command=command,
        returncode=returncode,
        duration_ms=duration_ms,
        log_path=log_path,
        workdir=workdir,
        error_message=error_message,
    )


def run_replay(
    *,
    arch: str,
    run_dir: Path,
    timeout_secs: int | None = None,
    idle_timeout_secs: int | None = None,
    image: Path | None = None,
    support_image: Path | None = None,
    skip_kernel_build: bool = True,
    keep_workdir: bool = False,
    interactive: bool = False,
    extra_block_image: Path | None = None,
    verbose: bool = False,
    workdir_override: Path | None = None,
    log_path_override: Path | None = None,
    kernel_override: Path | None = None,
) -> ReplayResult:
    selected_arch = normalize_arch(arch)
    root = repo_root()
    if not skip_kernel_build:
        subprocess.run(["make", kernel_name(selected_arch)], cwd=root, check=True)

    kernel = kernel_override.expanduser() if kernel_override is not None else root / kernel_name(selected_arch)
    if not kernel.is_file():
        raise ReplayError(f"missing kernel artifact: {kernel}")

    image_source = image.expanduser() if image is not None else find_official_image(selected_arch, root=root)
    prepared_image = prepare_image(image_source, root=root, verbose=verbose)

    support_source = support_image.expanduser() if support_image is not None else find_support_image(selected_arch, root=root)
    prepared_support = (
        prepare_image(support_source, root=root, verbose=verbose)
        if support_source is not None
        else None
    )
    prepared_extra = (
        prepare_image(extra_block_image.expanduser(), root=root, verbose=verbose)
        if extra_block_image is not None
        else None
    )

    arch_dir = run_dir / selected_arch
    workdir = workdir_override.expanduser() if workdir_override is not None else arch_dir / "work"
    if workdir.exists():
        shutil.rmtree(workdir)
    workdir.mkdir(parents=True, exist_ok=True)
    log_path = log_path_override.expanduser() if log_path_override is not None else arch_dir / "console.log"
    command = build_qemu_command(
        arch=selected_arch,
        kernel=kernel,
        image=prepared_image.runtime,
        support_image=prepared_support.runtime if prepared_support else None,
        extra_block_image=prepared_extra.runtime if prepared_extra else None,
    )

    result = run_qemu(
        arch=selected_arch,
        command=command,
        log_path=log_path,
        workdir=workdir,
        timeout_secs=timeout_secs,
        idle_timeout_secs=idle_timeout_secs,
        interactive=interactive,
    )
    if keep_workdir:
        log_copy = log_path.read_bytes() if log_path.is_file() else None
        shutil.rmtree(workdir, ignore_errors=True)
        workdir.mkdir(parents=True, exist_ok=True)
        if log_copy is not None:
            log_path.write_bytes(log_copy)
    else:
        shutil.rmtree(workdir, ignore_errors=True)
    return result


def normalize_arch(value: str) -> Arch:
    if value in {"rv", "riscv64"}:
        return "rv"
    if value in {"la", "loongarch64"}:
        return "la"
    raise ReplayError(f"unsupported arch: {value}")


def score_with_extra_issues(
    score: ScoreSummary,
    extra_issues: list[dict[str, object]],
) -> ScoreSummary:
    if not extra_issues:
        return score
    return dataclass_replace(score, issues=score.issues + tuple(extra_issues))


def build_run_provenance(
    *,
    name: str,
    mode: str,
    status: str,
    arches: tuple[str, ...],
    timeout_secs: int | None,
    idle_timeout_secs: int | None,
    plan_path: Path | None,
    support_image: Path | None,
    ltp_list: Path | None,
    replays: tuple[ReplayResult, ...],
) -> dict[str, Any]:
    run: dict[str, Any] = {
        "name": name,
        "mode": mode,
        "status": status,
        "arches": list(arches),
    }
    if timeout_secs is not None:
        run["timeout_secs"] = timeout_secs
    if idle_timeout_secs is not None:
        run["idle_timeout_secs"] = idle_timeout_secs
    if plan_path is not None:
        run["plan"] = str(plan_path)
    if support_image is not None:
        run["support_image"] = str(support_image)
    if ltp_list is not None:
        run["ltp_list"] = str(ltp_list)
    if replays:
        run["replays"] = [replay.to_json_dict() for replay in replays]
    return run


@dataclass(frozen=True)
class _ArchReplayOutcome:
    replay: ReplayResult
    judge_summary: JudgeSummary | None
    replay_issue: dict[str, object] | None


def _replay_and_judge_arch(
    *,
    selected_arch: Arch,
    run_dir: Path,
    timeout_secs: int | None,
    idle_timeout_secs: int | None,
    image: Path | None,
    support_image: Path | None,
    skip_kernel_build: bool,
    keep_workdir: bool,
    judge_dir: Path | None,
    judge_timeout_secs: float,
    effective_matrix: tuple[tuple[str, Libc], ...],
    verbose: bool,
) -> _ArchReplayOutcome:
    replay = run_replay(
        arch=selected_arch,
        run_dir=run_dir,
        timeout_secs=timeout_secs,
        idle_timeout_secs=idle_timeout_secs,
        image=image,
        support_image=support_image,
        skip_kernel_build=skip_kernel_build,
        keep_workdir=keep_workdir,
        verbose=verbose,
    )
    replay_issue: dict[str, object] | None = None
    if not replay.ok:
        replay_issue = {
            "kind": "replay-status",
            "arch": selected_arch,
            "returncode": replay.returncode,
            "log_path": str(replay.log_path),
        }
        if replay.error_message is not None:
            replay_issue["error"] = replay.error_message

    judge_summary: JudgeSummary | None = None
    if replay.log_path.is_file() and not replay.launch_failed:
        judge_summary = judge_log(
            log_path=replay.log_path,
            arch=selected_arch,
            out_dir=run_dir / selected_arch,
            judge_dir=judge_dir,
            judge_timeout_secs=judge_timeout_secs,
            fail_fast=False,
            group_libc_matrix=effective_matrix,
        )
    return _ArchReplayOutcome(
        replay=replay,
        judge_summary=judge_summary,
        replay_issue=replay_issue,
    )


def replay_status(replays: tuple[ReplayResult, ...], score: ScoreSummary) -> str:
    if any(replay.interrupted for replay in replays):
        return "interrupted"
    if any(replay.timed_out for replay in replays):
        return "timeout"
    if any(not replay.ok for replay in replays):
        return "replay-error"
    if score.has_errors:
        return "incomplete"
    return "complete"


def evaluate_replay(
    *,
    name: str,
    arch: str = "both",
    run_dir: Path | None = None,
    timeout_secs: int | None = None,
    idle_timeout_secs: int | None = None,
    image: Path | None = None,
    support_image: Path | None = None,
    ltp_list: Path | None = None,
    plan_path: Path | None = None,
    skip_kernel_build: bool = True,
    keep_workdir: bool = False,
    judge_dir: Path | None = None,
    judge_timeout_secs: float = JUDGE_TIMEOUT_SECS,
    fail_fast: bool = False,
    replace: bool = False,
    group_libc_matrix: tuple[tuple[str, Libc], ...] | None = None,
    verbose: bool = False,
) -> ReplayRunResult:
    if run_dir is None:
        run_dir = create_run_dir(name, replace=replace)
    else:
        run_dir = prepare_run_dir(run_dir, replace=replace)
    arches = tuple(normalize_arch(item) for item in expand_arches(arch))
    effective_matrix = effective_group_libc_matrix(group_libc_matrix)
    if support_image is not None and ltp_list is not None:
        raise ValueError("--support-image and --ltp-list cannot be combined")
    if ltp_list is not None and not ltp_list.is_file():
        raise ValueError(f"ltp list does not exist: {ltp_list}")
    if plan_path is not None and not plan_path.is_file():
        raise ValueError(f"plan does not exist: {plan_path}")

    support_image_build: SupportImageBuild | None = None
    if ltp_list is not None:
        support_arch = "both" if len(arches) > 1 else arches[0]
        support_image_build = build_support_image(
            arch=support_arch,
            run_dir=run_dir,
            ltp_list=ltp_list,
            plan=plan_path,
        )
        support_image = support_image_build.output_path

    replay_kwargs = {
        "run_dir": run_dir,
        "timeout_secs": timeout_secs,
        "idle_timeout_secs": idle_timeout_secs,
        "image": image,
        "support_image": support_image,
        "skip_kernel_build": skip_kernel_build,
        "keep_workdir": keep_workdir,
        "judge_dir": judge_dir,
        "judge_timeout_secs": judge_timeout_secs,
        "effective_matrix": effective_matrix,
        "verbose": verbose,
    }

    outcomes: list[_ArchReplayOutcome] = []
    if len(arches) == 1 or fail_fast:
        for selected_arch in arches:
            outcomes.append(
                _replay_and_judge_arch(selected_arch=selected_arch, **replay_kwargs)
            )
            if outcomes[-1].replay_issue is not None:
                break
    else:
        with ThreadPoolExecutor(max_workers=len(arches)) as pool:
            futures = {
                pool.submit(
                    _replay_and_judge_arch,
                    selected_arch=selected_arch,
                    **replay_kwargs,
                ): selected_arch
                for selected_arch in arches
            }
            for future in as_completed(futures):
                outcomes.append(future.result())
        arch_order = {arch: index for index, arch in enumerate(arches)}
        outcomes.sort(key=lambda outcome: arch_order.get(outcome.replay.arch, len(arches)))

    replays: list[ReplayResult] = []
    judge_summaries: list[JudgeSummary] = []
    replay_issues: list[dict[str, object]] = []
    for outcome in outcomes:
        replays.append(outcome.replay)
        if outcome.replay_issue is not None:
            replay_issues.append(outcome.replay_issue)
            if fail_fast:
                break
        if outcome.judge_summary is not None:
            judge_summaries.append(outcome.judge_summary)

    score = score_judge_summaries(judge_summaries)
    score = score_with_extra_issues(score, replay_issues)
    status = replay_status(tuple(replays), score)
    score = dataclass_replace(
        score,
        run=build_run_provenance(
            name=name,
            mode="replay",
            status=status,
            arches=arches,
            timeout_secs=timeout_secs,
            idle_timeout_secs=idle_timeout_secs,
            plan_path=plan_path,
            support_image=support_image,
            ltp_list=ltp_list,
            replays=tuple(replays),
        ),
    )
    write_score_summary(score, run_dir / "score.json")
    return ReplayRunResult(
        run_dir=run_dir,
        replays=tuple(replays),
        judge_summaries=tuple(judge_summaries),
        score=score,
        status=status,
        support_image_build=support_image_build,
    )


def run_shell(
    *,
    arch: Arch,
    kernel: Path | None = None,
    image: Path | None = None,
    support_image: Path | None = None,
    extra_block_image: Path | None = None,
    workdir: Path | None = None,
    log_path: Path | None = None,
    timeout_secs: int | None = 0,
    verbose: bool = False,
) -> ReplayResult:
    root = repo_root()
    run_dir = prepare_run_dir(state_root(root) / "shell" / arch, replace=True)
    return run_replay(
        arch=arch,
        run_dir=run_dir,
        timeout_secs=timeout_secs,
        idle_timeout_secs=None,
        image=image,
        support_image=support_image,
        skip_kernel_build=True,
        keep_workdir=True,
        interactive=True,
        extra_block_image=extra_block_image,
        verbose=verbose,
        workdir_override=workdir,
        log_path_override=log_path,
        kernel_override=kernel,
    )


def exit_code_for_replay(result: ReplayRunResult) -> int:
    if result.interrupted:
        return 130
    if result.timed_out:
        return 124
    if result.replay_failures and not result.judge_summaries:
        return 3
    if result.replay_failures or result.score.has_errors:
        return 1
    return 0


def replay_cmd(args: argparse.Namespace) -> int:
    try:
        group_libc_matrix = group_libc_matrix_from_plan(Path(args.plan).expanduser()) if args.plan else None
        result = evaluate_replay(
            name=args.name or f"replay-{args.arch}",
            arch=args.arch,
            run_dir=Path(args.out).expanduser() if args.out else None,
            timeout_secs=args.timeout,
            idle_timeout_secs=args.idle_timeout,
            image=Path(args.image).expanduser() if args.image else None,
            support_image=Path(args.support_image).expanduser() if args.support_image else None,
            ltp_list=Path(args.ltp_list).expanduser() if args.ltp_list else None,
            plan_path=Path(args.plan).expanduser() if args.plan else None,
            skip_kernel_build=True,
            keep_workdir=False,
            judge_timeout_secs=args.judge_timeout,
            replace=True,
            group_libc_matrix=group_libc_matrix,
            verbose=args.verbose or truthy(os.environ.get("OSCOMP_REPLAY_VERBOSE")),
        )
    except (ReplayError, ValueError, FileExistsError, OSError, MarkerError, JudgeRunnerError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(
        "replay "
        f"run_dir={result.run_dir} "
        f"status={result.status} "
        f"total={result.score.total_score:.6g} "
        f"issues={len(result.score.issues)} "
        f"replay_failures={result.replay_failures}"
    )
    return exit_code_for_replay(result)


def shell_cmd(args: argparse.Namespace) -> int:
    try:
        result = run_shell(
            arch=normalize_arch(args.arch),
            kernel=Path(args.kernel).expanduser() if args.kernel else None,
            image=Path(args.image).expanduser() if args.image else None,
            support_image=Path(args.support_image).expanduser() if args.support_image else None,
            extra_block_image=Path(args.extra_block_image).expanduser() if args.extra_block_image else None,
            workdir=Path(args.workdir).expanduser() if args.workdir else None,
            log_path=Path(args.log).expanduser() if args.log else None,
            timeout_secs=args.timeout,
            verbose=args.verbose or truthy(os.environ.get("OSCOMP_REPLAY_VERBOSE")),
        )
    except (ReplayError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    if result.interrupted:
        return 130
    if result.timed_out:
        return 124
    return 0 if result.ok else 1


def qemu_cmd(args: argparse.Namespace) -> int:
    try:
        run_dir = Path(args.workdir).expanduser().parent if args.workdir else state_root(repo_root()) / "qemu" / args.arch
        log_path = Path(args.log).expanduser() if args.log else None
        if log_path is None and args.workdir:
            log_path = Path(args.workdir).expanduser() / "qemu.log"
        result = run_replay(
            arch=args.arch,
            run_dir=run_dir,
            timeout_secs=args.timeout,
            idle_timeout_secs=args.idle_timeout,
            image=Path(args.image).expanduser() if args.image else None,
            support_image=Path(args.support_image).expanduser() if args.support_image else None,
            skip_kernel_build=args.skip_kernel_build,
            keep_workdir=args.keep_workdir,
            interactive=args.interactive,
            extra_block_image=Path(args.extra_block_image).expanduser() if args.extra_block_image else None,
            verbose=args.verbose or truthy(os.environ.get("OSCOMP_REPLAY_VERBOSE")),
            workdir_override=Path(args.workdir).expanduser() if args.workdir else None,
            log_path_override=log_path,
            kernel_override=Path(args.kernel).expanduser() if args.kernel else None,
        )
    except (ReplayError, OSError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    if result.interrupted:
        return 130
    if result.timed_out:
        return 124
    return 0 if result.ok else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="python3 -m tools.oscomp_eval.replay")
    subparsers = parser.add_subparsers(dest="command", required=True)

    replay_parser = subparsers.add_parser("replay", help="run QEMU, judge, score, and report")
    replay_parser.add_argument("--arch", required=True, choices=("rv", "la"))
    replay_parser.add_argument("--image", help="official testsuite image override")
    replay_parser.add_argument("--support-image", help="support disk image override")
    replay_parser.add_argument("--ltp-list", help="build run-local support image with this LTP list")
    replay_parser.add_argument("--plan", help="focused group/libc plan")
    replay_parser.add_argument("--timeout", type=int, default=REPLAY_TIMEOUT_FULL_SECS)
    replay_parser.add_argument("--idle-timeout", type=int)
    replay_parser.add_argument("--judge-timeout", type=float, default=JUDGE_TIMEOUT_SECS)
    replay_parser.add_argument("--name", help="run name; default replay-ARCH")
    replay_parser.add_argument("--out", help="explicit run directory")
    replay_parser.add_argument("--verbose", action="store_true")
    replay_parser.set_defaults(func=replay_cmd)

    shell_parser = subparsers.add_parser("shell", help="boot an interactive shell-mode kernel")
    shell_parser.add_argument("--arch", required=True, choices=("rv", "la"))
    shell_parser.add_argument("--kernel", help="kernel ELF to boot; defaults to kernel-rv/kernel-la")
    shell_parser.add_argument("--image", help="official testsuite image override")
    shell_parser.add_argument("--support-image", help="support disk image override")
    shell_parser.add_argument("--extra-block-image", help="additional writable raw block image")
    shell_parser.add_argument("--workdir", help="temporary run directory")
    shell_parser.add_argument("--log", help="console log path")
    shell_parser.add_argument("--timeout", type=int, default=SHELL_TIMEOUT_SECS)
    shell_parser.add_argument("--verbose", action="store_true")
    shell_parser.set_defaults(func=shell_cmd)

    qemu_parser = subparsers.add_parser("qemu", help=argparse.SUPPRESS)
    qemu_parser.add_argument("--arch", required=True, choices=("rv", "la"))
    qemu_parser.add_argument("--kernel", help="kernel ELF to boot; defaults to kernel-rv/kernel-la")
    qemu_parser.add_argument("--image", help="official testsuite image override")
    qemu_parser.add_argument("--support-image", help="support disk image override")
    qemu_parser.add_argument("--extra-block-image", help="additional writable raw block image")
    qemu_parser.add_argument("--timeout", type=int, default=REPLAY_TIMEOUT_FULL_SECS)
    qemu_parser.add_argument("--idle-timeout", type=int)
    qemu_parser.add_argument("--workdir", help="temporary run directory")
    qemu_parser.add_argument("--log", help="console log path")
    qemu_parser.add_argument("--skip-kernel-build", action="store_true")
    qemu_parser.add_argument("--keep-workdir", action="store_true")
    qemu_parser.add_argument("--interactive", action="store_true")
    qemu_parser.add_argument("--verbose", action="store_true")
    qemu_parser.set_defaults(func=qemu_cmd)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
        return args.func(args)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
