"""Thin replay orchestration around scripts/replay-oscomp-eval.sh."""

from __future__ import annotations

import os
import signal
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from .paths import repo_root


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


def run_replay(
    *,
    arch: str,
    run_dir: Path,
    timeout_secs: int | None = None,
    idle_timeout_secs: int | None = None,
    image: Path | None = None,
    support_image: Path | None = None,
    skip_kernel_build: bool = False,
    keep_workdir: bool = False,
    runner_path: Path | None = None,
) -> ReplayResult:
    root = repo_root()
    arch_dir = run_dir / arch
    arch_dir.mkdir(parents=True, exist_ok=True)
    workdir = arch_dir / "replay-workdir"
    log_path = arch_dir / "console.log"
    runner = runner_path or (root / "scripts" / "replay-oscomp-eval.sh")

    command = [
        str(runner),
        "--arch",
        arch,
        "--workdir",
        str(workdir),
        "--log",
        str(log_path),
    ]
    if timeout_secs is not None:
        command.extend(["--timeout", str(timeout_secs)])
    if image is not None:
        command.extend(["--image", str(image)])
    if support_image is not None:
        command.extend(["--support-image", str(support_image)])
    if skip_kernel_build:
        command.append("--skip-kernel-build")
    if keep_workdir:
        command.append("--keep-workdir")

    def append_log(message: str) -> None:
        try:
            log_path.parent.mkdir(parents=True, exist_ok=True)
            with log_path.open("a", encoding="utf-8") as log_file:
                log_file.write(f"[oscomp-eval] {message}\n")
        except OSError:
            pass

    def terminate_process_group(process: subprocess.Popen[object], sig: int) -> None:
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        except OSError:
            process.terminate()

    start = time.monotonic()
    try:
        process = subprocess.Popen(command, start_new_session=True)
        returncode: int | None = None
        error_message = None
        last_output_at = start
        last_log_size = -1
        last_log_mtime_ns = -1
        while True:
            returncode = process.poll()
            if returncode is not None:
                break
            if log_path.exists():
                try:
                    stat = log_path.stat()
                except OSError:
                    stat = None
                if stat is not None and (
                    stat.st_size != last_log_size
                    or stat.st_mtime_ns != last_log_mtime_ns
                ):
                    last_log_size = stat.st_size
                    last_log_mtime_ns = stat.st_mtime_ns
                    last_output_at = time.monotonic()
            if (
                idle_timeout_secs is not None
                and idle_timeout_secs > 0
                and time.monotonic() - last_output_at >= idle_timeout_secs
            ):
                error_message = (
                    f"replay idle timeout after {idle_timeout_secs}s without console output"
                )
                append_log(error_message)
                terminate_process_group(process, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    terminate_process_group(process, signal.SIGKILL)
                    process.wait()
                returncode = 124
                break
            time.sleep(0.5)
        if returncode is None:
            returncode = process.returncode
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
        append_log(error_message)
    duration_ms = int((time.monotonic() - start) * 1000)
    return ReplayResult(
        arch=arch,
        command=tuple(command),
        returncode=returncode,
        duration_ms=duration_ms,
        log_path=log_path,
        workdir=workdir,
        error_message=error_message,
    )
