"""Versioned QEMU receipt helpers, including external stdin finalization."""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
from pathlib import Path
from typing import Any

from .evidence import EvidenceError, validate_file_evidence
from .model import InputForwarding


RECEIPT_SCHEMA_VERSION = 2


class ReceiptError(ValueError):
    """Raised when a QEMU receipt transition or input record is invalid."""


def atomic_write_receipt(path: Path, payload: dict[str, Any]) -> None:
    path = path.expanduser().resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
    )
    temporary = Path(temporary_name)
    try:
        output = os.fdopen(descriptor, "w", encoding="utf-8")
        descriptor = -1
        with output:
            json.dump(payload, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        temporary.unlink(missing_ok=True)


def command_stream_evidence(path: Path) -> dict[str, str | int]:
    """Hash the command artifact and count logical input lines."""

    path = path.expanduser().resolve()
    digest = hashlib.sha256()
    byte_count = 0
    newline_count = 0
    last_byte: int | None = None
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
                byte_count += len(chunk)
                newline_count += chunk.count(b"\n")
                last_byte = chunk[-1]
    except OSError as error:
        raise ReceiptError(f"cannot read command stream {path}: {error}") from error
    line_count = newline_count
    if byte_count > 0 and last_byte != ord("\n"):
        line_count += 1
    return {
        "sha256": digest.hexdigest(),
        "bytes": byte_count,
        "line_count": line_count,
    }


def input_forwarding_payload(forwarding: InputForwarding) -> dict[str, Any]:
    return {
        "state": "awaiting_producer",
        "sha256": forwarding.sha256,
        "bytes": forwarding.bytes_forwarded,
        "line_count": forwarding.line_count,
        "observed_bytes": forwarding.observed_bytes,
        "source_eof": forwarding.source_eof,
        "broken_pipe": forwarding.broken_pipe,
        "relay_complete": forwarding.relay_complete,
    }


def _require(condition: bool, message: str) -> None:
    if not condition:
        raise ReceiptError(message)


def _require_sha256(value: Any, label: str) -> str:
    _require(
        isinstance(value, str)
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value),
        f"invalid {label} SHA-256",
    )
    return value


def _require_nonnegative_int(value: Any, label: str) -> int:
    _require(type(value) is int and value >= 0, f"invalid {label}")
    return value


def validate_receipt_file_evidence(
    receipt: dict[str, Any],
) -> dict[str, dict[str, Any]]:
    """Validate relocation-safe file evidence and its internal relationships."""

    records: dict[str, dict[str, Any]] = {}
    for key in (
        "kernel",
        "rootfs_source",
        "rootfs_runtime_before",
        "rootfs_runtime_after",
        "qemu",
        "log",
    ):
        try:
            records[key] = validate_file_evidence(receipt.get(key), key)
        except EvidenceError as error:
            raise ReceiptError(str(error)) from error

    _require(
        receipt.get("log_path") == records["log"]["path"],
        "QEMU receipt log_path does not match log evidence",
    )
    rootfs_mode = receipt.get("rootfs_mode")
    _require(
        rootfs_mode in ("snapshot", "readonly", "rw"),
        "invalid QEMU receipt rootfs mode",
    )
    rootfs_before = records["rootfs_runtime_before"]
    rootfs_after = records["rootfs_runtime_after"]
    _require(
        rootfs_before["path"] == rootfs_after["path"],
        "runtime rootfs path changed during QEMU",
    )
    if rootfs_mode != "rw":
        _require(
            rootfs_before == rootfs_after,
            "read-only runtime rootfs changed during QEMU",
        )
    return records


def _producer_status_kind(status: int) -> str:
    if status < 128:
        return f"exit:{status}"
    return f"signal:{status - 128}"


def finalize_external_input_receipt(
    *,
    receipt_path: Path,
    commands_path: Path,
    expected_sha256: str,
    expected_bytes: int,
    expected_line_count: int,
    producer_status: int,
) -> bool:
    """Finalize the wrapper-owned producer facts after the pipeline exits.

    The returned boolean is the acceptance decision.  Even an exact forwarded
    stream does not make SIGPIPE 141 a successful producer termination: exact
    evidence requires both a complete relay and a normal producer exit.
    """

    _require_sha256(expected_sha256, "expected command stream")
    _require(expected_bytes >= 0, "invalid expected command byte count")
    _require(expected_line_count >= 0, "invalid expected command line count")
    _require(0 <= producer_status <= 255, "invalid producer status")

    receipt_path = receipt_path.expanduser().resolve()
    try:
        with receipt_path.open(encoding="utf-8") as source:
            loaded: Any = json.load(source)
    except (OSError, json.JSONDecodeError) as error:
        raise ReceiptError(f"cannot read QEMU receipt {receipt_path}: {error}") from error
    _require(isinstance(loaded, dict), "QEMU receipt must be a JSON object")
    receipt: dict[str, Any] = loaded
    _require(
        receipt.get("schema_version") == RECEIPT_SCHEMA_VERSION,
        "unsupported QEMU receipt schema",
    )
    _require(
        receipt.get("state") == "awaiting_producer",
        "QEMU receipt is not awaiting an external producer",
    )
    interaction = receipt.get("interaction")
    _require(
        isinstance(interaction, dict)
        and interaction.get("external_input_producer") is True,
        "QEMU receipt does not own an external input producer",
    )
    value = receipt.get("stdin")
    _require(isinstance(value, dict), "QEMU receipt lacks stdin forwarding evidence")
    stdin: dict[str, Any] = value
    _require(stdin.get("state") == "awaiting_producer", "stdin receipt is not awaiting producer")

    forwarded_sha256 = _require_sha256(stdin.get("sha256"), "forwarded stdin")
    forwarded_bytes = _require_nonnegative_int(stdin.get("bytes"), "forwarded stdin bytes")
    forwarded_lines = _require_nonnegative_int(
        stdin.get("line_count"), "forwarded stdin line count"
    )
    observed_bytes = _require_nonnegative_int(
        stdin.get("observed_bytes"), "observed stdin bytes"
    )
    for field in ("source_eof", "broken_pipe", "relay_complete"):
        _require(type(stdin.get(field)) is bool, f"invalid stdin {field}")

    current = command_stream_evidence(commands_path)
    expected = {
        "sha256": expected_sha256,
        "bytes": expected_bytes,
        "line_count": expected_line_count,
    }
    source_unchanged = current == expected
    forwarded_matches_source = (
        forwarded_sha256 == expected_sha256
        and forwarded_bytes == expected_bytes
        and forwarded_lines == expected_line_count
    )
    source_fully_relayed = (
        source_unchanged
        and forwarded_matches_source
        and observed_bytes == expected_bytes
        and stdin["source_eof"] is True
        and stdin["broken_pipe"] is False
        and stdin["relay_complete"] is True
    )
    producer_status_accepted = source_fully_relayed and producer_status == 0

    stdin.update(
        {
            "state": "complete",
            "source_sha256": expected_sha256,
            "source_bytes": expected_bytes,
            "source_line_count": expected_line_count,
            "source_unchanged": source_unchanged,
            "producer_status": producer_status,
            "producer_status_kind": _producer_status_kind(producer_status),
            "source_fully_relayed": source_fully_relayed,
            "producer_status_accepted": producer_status_accepted,
        }
    )
    receipt["state"] = "complete"
    atomic_write_receipt(receipt_path, receipt)
    return producer_status_accepted


def validate_completed_input_receipt(
    receipt: dict[str, Any], commands_path: Path
) -> dict[str, Any]:
    """Validate final stdin evidence against the exact command artifact."""

    _require(
        receipt.get("schema_version") == RECEIPT_SCHEMA_VERSION,
        "unsupported QEMU receipt schema",
    )
    _require(receipt.get("state") == "complete", "QEMU receipt is not complete")
    interaction = receipt.get("interaction")
    _require(
        isinstance(interaction, dict)
        and interaction.get("external_input_producer") is True,
        "QEMU receipt does not own an external input producer",
    )
    value = receipt.get("stdin")
    _require(isinstance(value, dict), "QEMU receipt lacks stdin forwarding evidence")
    stdin: dict[str, Any] = value
    _require(stdin.get("state") == "complete", "stdin receipt is not complete")

    expected = command_stream_evidence(commands_path)
    for field in ("sha256", "source_sha256"):
        _require_sha256(stdin.get(field), field)
        _require(stdin[field] == expected["sha256"], f"stdin {field} mismatch")
    for field in ("bytes", "source_bytes", "observed_bytes"):
        _require_nonnegative_int(stdin.get(field), field)
        _require(stdin[field] == expected["bytes"], f"stdin {field} mismatch")
    for field in ("line_count", "source_line_count"):
        _require_nonnegative_int(stdin.get(field), field)
        _require(stdin[field] == expected["line_count"], f"stdin {field} mismatch")
    for field in (
        "source_eof",
        "relay_complete",
        "source_unchanged",
        "source_fully_relayed",
        "producer_status_accepted",
    ):
        _require(stdin.get(field) is True, f"stdin {field} is not true")
    _require(stdin.get("broken_pipe") is False, "stdin forwarding hit a broken pipe")
    producer_status = _require_nonnegative_int(
        stdin.get("producer_status"), "producer status"
    )
    _require(producer_status == 0, "producer status is not a normal exit")
    _require(
        stdin.get("producer_status_kind") == _producer_status_kind(producer_status),
        "producer status kind mismatch",
    )
    return stdin
