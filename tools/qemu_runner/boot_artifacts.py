"""Check the actual kernel payload selected by the product's UEFI layout."""
from pathlib import Path
import re
import subprocess

from .runner import RunnerError


def validate_esp_kernel(kernel: Path, esp: Path, payload_path: str) -> None:
    """Reject a stale ESP without rebuilding or retaining extra files."""
    try:
        expected = kernel.read_bytes()
        if not expected:
            raise RunnerError(f"kernel is empty: {kernel}")
        payload = subprocess.run(
            ["mtype", "-i", f"{esp}@@1M", payload_path],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False, timeout=30,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RunnerError(f"cannot verify ESP kernel payload: {esp}: {error}") from error
    if payload.returncode:
        detail = payload.stderr.decode("utf-8", errors="replace").strip()
        raise RunnerError(f"cannot read ESP kernel payload: {esp}: {detail}")
    if payload.stdout != expected:
        raise RunnerError(f"ESP does not contain the requested kernel; rebuild: {esp}; payload={payload_path}; kernel={kernel}")


def validate_linux_esp_kernel(kernel: Path, esp: Path) -> None:
    validate_esp_kernel(kernel, esp, "::/vmlinuz")


def validate_thekernel_esp_kernel(kernel: Path, esp: Path) -> None:
    validate_esp_kernel(kernel, esp, "::/TheKernel.elf")


def validate_linux_boot(text: str, log_path: Path) -> None:
    versions = re.findall(r"^(?:\[\s*\d+\.\d+\]\s+)?Linux version (\S+)", text, re.MULTILINE)
    if versions != ["7.2.3"]:
        raise RunnerError(f"oracle did not identify exactly one Linux 7.2.3 boot: {log_path}")
