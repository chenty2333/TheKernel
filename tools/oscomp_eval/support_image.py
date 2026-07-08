"""Support image build helpers for replay-backed evaluation."""

from __future__ import annotations

import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

from .paths import repo_root


class SupportImageError(RuntimeError):
    """Raised when a generated support image cannot be prepared."""


class SupportImageConfigError(SupportImageError, ValueError):
    """Raised when support image build inputs are invalid."""


@dataclass(frozen=True)
class SupportImageInspection:
    arch: str
    image: Path
    ok: bool
    issues: tuple[str, ...]

    def to_json_dict(self) -> dict[str, object]:
        return {
            "arch": self.arch,
            "image": str(self.image),
            "ok": self.ok,
            "issues": list(self.issues),
        }


@dataclass(frozen=True)
class SupportImageBuild:
    arch: str
    command: tuple[str, ...]
    returncode: int
    duration_ms: int
    output_path: Path
    ltp_list: Path
    plan: Path | None

    def to_json_dict(self) -> dict[str, object]:
        return {
            "arch": self.arch,
            "command": list(self.command),
            "returncode": self.returncode,
            "duration_ms": self.duration_ms,
            "output_path": str(self.output_path),
            "ltp_list": str(self.ltp_list),
            "plan": str(self.plan) if self.plan is not None else None,
        }


def _debugfs_capture(image: Path, command: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["debugfs", "-R", command, str(image)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def _debugfs_cat(image: Path, path: str) -> tuple[str | None, str | None]:
    try:
        completed = _debugfs_capture(image, f"cat {path}")
    except OSError as error:
        return None, f"could not run debugfs: {error}"
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()
        return None, detail or f"debugfs cat failed for {path}"
    return completed.stdout, None


def _debugfs_exists(image: Path, path: str) -> tuple[bool, str | None]:
    try:
        completed = _debugfs_capture(image, f"stat {path}")
    except OSError as error:
        return False, f"could not run debugfs: {error}"
    detail = (completed.stderr or completed.stdout).strip()
    combined = f"{completed.stdout}\n{completed.stderr}".lower()
    if "not found" in combined or "no such file" in combined:
        return False, detail or f"debugfs stat failed for {path}"
    if completed.returncode != 0:
        return False, detail or f"debugfs stat failed for {path}"
    return True, None


def inspect_support_image(
    *,
    arch: str,
    image: Path,
    root: Path | None = None,
) -> SupportImageInspection:
    if arch not in ("rv", "la"):
        raise SupportImageConfigError(f"unsupported support-image arch: {arch}")

    root = root or repo_root()
    issues: list[str] = []
    if not image.is_file():
        return SupportImageInspection(
            arch=arch,
            image=image,
            ok=False,
            issues=(f"support image does not exist: {image}",),
        )

    init_exists, _ = _debugfs_exists(image, "/meta/init.sh")
    if init_exists:
        init_text, error = _debugfs_cat(image, "/meta/init.sh")
    else:
        init_text, error = None, "optional init is absent"
    if init_exists and error is None and init_text is not None:
        current_init = (root / "src" / "init.sh").read_text(encoding="utf-8")
        if init_text != current_init:
            issues.append("optional /meta/init.sh does not match current src/init.sh")

    for required_path in (
        "/meta/ltp_test.txt",
        f"/{arch}/overlay/bin/oscomp-timeout",
    ):
        exists, error = _debugfs_exists(image, required_path)
        if not exists:
            issues.append(f"missing {required_path}: {error}")

    return SupportImageInspection(
        arch=arch,
        image=image,
        ok=not issues,
        issues=tuple(issues),
    )


def build_support_image(
    *,
    arch: str,
    run_dir: Path,
    ltp_list: Path,
    plan: Path | None = None,
) -> SupportImageBuild:
    if arch not in ("rv", "la", "both"):
        raise SupportImageConfigError(f"unsupported support-image arch: {arch}")
    if not ltp_list.is_file():
        raise SupportImageConfigError(f"ltp list does not exist: {ltp_list}")
    if plan is not None and not plan.is_file():
        raise SupportImageConfigError(f"plan does not exist: {plan}")

    root = repo_root()
    inputs_dir = run_dir / "inputs"
    inputs_dir.mkdir(parents=True, exist_ok=True)
    output_path = inputs_dir / f"support-{arch}.img"
    builder = root / "scripts" / "build-oscomp-support-disk.sh"
    command = [
        str(builder),
        "--arch",
        arch,
        "--output",
        str(output_path),
        "--test-list",
        str(ltp_list),
    ]
    if plan is not None:
        command.extend(["--plan-override", str(plan)])

    start = time.monotonic()
    try:
        completed = subprocess.run(command, check=False)
    except OSError as error:
        raise SupportImageError(f"could not run support image builder: {builder}") from error
    duration_ms = int((time.monotonic() - start) * 1000)

    if completed.returncode != 0:
        raise SupportImageError(
            f"support image build failed with exit code {completed.returncode}"
        )
    if not output_path.is_file():
        raise SupportImageError(
            f"support image builder did not create expected output: {output_path}"
        )

    return SupportImageBuild(
        arch=arch,
        command=tuple(command),
        returncode=completed.returncode,
        duration_ms=duration_ms,
        output_path=output_path,
        ltp_list=ltp_list,
        plan=plan,
    )
