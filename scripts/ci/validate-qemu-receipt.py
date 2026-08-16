#!/usr/bin/env python3
"""Validate a completed QEMU receipt against the exact guest inputs."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from tools.qemu_runner.command import build_qemu_command  # noqa: E402
from tools.qemu_runner.evidence import (  # noqa: E402
    EvidenceError as FileEvidenceError,
    file_evidence,
    validate_file_evidence,
)
from tools.qemu_runner.model import Drive  # noqa: E402
from tools.qemu_runner.receipt import (  # noqa: E402
    RECEIPT_SCHEMA_VERSION,
    ReceiptError,
    validate_completed_input_receipt,
    validate_receipt_file_evidence,
)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def evidence_record(value: Any, label: str) -> dict[str, Any]:
    try:
        return validate_file_evidence(value, label)
    except FileEvidenceError as error:
        raise ValueError(str(error)) from error


def current_evidence(record: dict[str, Any], label: str) -> dict[str, Any]:
    path = record.get("path")
    require(isinstance(path, str), f"missing {label} path")
    try:
        return file_evidence(Path(path))
    except (OSError, FileEvidenceError) as error:
        raise ValueError(str(error)) from error


def validate_boot_mode(
    args: argparse.Namespace, receipt: dict[str, Any]
) -> tuple[Drive | None, Path | None, Path | None]:
    direct_kernel = receipt.get("direct_kernel")
    require(type(direct_kernel) is bool, "invalid receipt direct-kernel boot mode")
    require(
        direct_kernel == args.direct_kernel,
        "direct-kernel mode does not match the receipt",
    )

    uefi_keys = (
        "esp_source",
        "esp_runtime",
        "ovmf_code",
        "ovmf_vars_source",
        "ovmf_vars_runtime",
    )
    if direct_kernel:
        require(
            all(key not in receipt for key in uefi_keys),
            "direct-kernel receipt unexpectedly contains UEFI evidence",
        )
        require(
            args.esp is None and args.ovmf_code is None and args.ovmf_vars is None,
            "UEFI inputs are not valid with --direct-kernel",
        )
        return None, None, None

    esp_source = evidence_record(receipt.get("esp_source"), "ESP source")
    esp_runtime = evidence_record(receipt.get("esp_runtime"), "ESP runtime")
    ovmf_code = evidence_record(receipt.get("ovmf_code"), "OVMF code")
    ovmf_vars_source = evidence_record(
        receipt.get("ovmf_vars_source"), "OVMF vars source"
    )
    ovmf_vars_runtime = evidence_record(
        receipt.get("ovmf_vars_runtime"), "OVMF vars runtime"
    )

    for record, label in (
        (esp_source, "ESP source"),
        (esp_runtime, "ESP runtime"),
        (ovmf_code, "OVMF code"),
        (ovmf_vars_source, "OVMF vars source"),
    ):
        require(
            current_evidence(record, label) == record,
            f"{label} evidence mismatch",
        )
    # OVMF writes its variable store while the guest runs.  The runner records
    # the runtime path before launch, so only require that the recorded path is
    # still a file; its post-boot contents are guest-owned state.
    current_evidence(ovmf_vars_runtime, "OVMF vars runtime")

    if args.esp is not None:
        require(
            esp_source == file_evidence(Path(args.esp)),
            "ESP source evidence mismatch",
        )
    if args.ovmf_code is not None:
        require(
            ovmf_code == file_evidence(Path(args.ovmf_code)),
            "OVMF code evidence mismatch",
        )
    if args.ovmf_vars is not None:
        require(
            ovmf_vars_source == file_evidence(Path(args.ovmf_vars)),
            "OVMF vars source evidence mismatch",
        )

    esp_runtime_path = esp_runtime.get("path")
    ovmf_code_path = ovmf_code.get("path")
    ovmf_vars_runtime_path = ovmf_vars_runtime.get("path")
    require(isinstance(esp_runtime_path, str), "missing ESP runtime path")
    require(isinstance(ovmf_code_path, str), "missing OVMF code path")
    require(isinstance(ovmf_vars_runtime_path, str), "missing OVMF vars runtime path")
    return (
        Drive(Path(esp_runtime_path), "snapshot"),
        Path(ovmf_code_path),
        Path(ovmf_vars_runtime_path),
    )


def validate(args: argparse.Namespace) -> None:
    receipt_path = Path(args.receipt).expanduser().resolve()
    with receipt_path.open(encoding="utf-8") as source:
        loaded: Any = json.load(source)
    require(isinstance(loaded, dict), "QEMU receipt must be a JSON object")
    receipt: dict[str, Any] = loaded

    require(
        type(receipt.get("schema_version")) is int
        and receipt["schema_version"] == RECEIPT_SCHEMA_VERSION,
        "unsupported receipt schema",
    )
    require(receipt.get("state") == "complete", "QEMU receipt is not complete")
    require(receipt.get("arch") == args.arch, "receipt architecture mismatch")
    require(
        type(receipt.get("cpus")) is int and receipt["cpus"] == args.cpus,
        "receipt CPU count mismatch",
    )
    require(
        type(receipt.get("returncode")) is int and receipt["returncode"] == 0,
        "QEMU receipt is not successful",
    )
    require(receipt.get("memory") == args.memory, "receipt memory mismatch")
    require(receipt.get("error_message") is None, "successful receipt contains an error")
    require(receipt.get("timed_out") is False, "successful receipt is marked timed out")
    require(receipt.get("interrupted") is False, "successful receipt is marked interrupted")
    require(
        receipt.get("intentionally_stopped") is False,
        "successful receipt is marked intentionally stopped",
    )
    require(
        type(receipt.get("duration_ms")) is int and receipt["duration_ms"] >= 0,
        "invalid QEMU duration",
    )

    esp, ovmf_code, ovmf_vars = validate_boot_mode(args, receipt)

    receipt_evidence = validate_receipt_file_evidence(receipt)
    kernel = file_evidence(Path(args.kernel))
    require(receipt_evidence["kernel"] == kernel, "kernel evidence mismatch")
    rootfs_source = receipt_evidence["rootfs_source"]
    expected_rootfs_path = str(Path(args.rootfs).expanduser().resolve())
    require(rootfs_source.get("path") == expected_rootfs_path, "rootfs path mismatch")
    rootfs_runtime_before = receipt_evidence["rootfs_runtime_before"]
    rootfs_runtime_after = receipt_evidence["rootfs_runtime_after"]
    rootfs_runtime_path = rootfs_runtime_before.get("path")
    require(isinstance(rootfs_runtime_path, str), "missing runtime rootfs path")
    require(
        rootfs_runtime_after == file_evidence(Path(rootfs_runtime_path)),
        "post-run rootfs evidence mismatch",
    )
    rootfs_mode = receipt.get("rootfs_mode")
    require(rootfs_mode == args.rootfs_mode, "rootfs mode mismatch")
    if rootfs_mode != "rw" or rootfs_runtime_path != expected_rootfs_path:
        require(
            rootfs_source == file_evidence(Path(expected_rootfs_path)),
            "rootfs source evidence mismatch",
        )
    else:
        require(
            rootfs_source == rootfs_runtime_before,
            "in-place writable rootfs has inconsistent pre-run evidence",
        )
    qemu = receipt_evidence["qemu"]
    qemu_path = qemu.get("path")
    require(isinstance(qemu_path, str), "missing resolved QEMU path")
    expected_qemu = file_evidence(Path(args.qemu_binary))
    require(qemu_path == expected_qemu["path"], "QEMU path mismatch")
    require(
        qemu.get("size_bytes") == expected_qemu["size_bytes"],
        "QEMU size mismatch",
    )
    require(qemu.get("sha256") == expected_qemu["sha256"], "QEMU hash mismatch")

    extra_block = None
    extra_source = receipt.get("extra_block_source")
    extra_before = receipt.get("extra_block_runtime_before")
    extra_after = receipt.get("extra_block_runtime_after")
    receipt_has_extra = any(value is not None for value in (extra_source, extra_before, extra_after))
    expected_has_extra = args.extra_block is not None
    require(receipt_has_extra == expected_has_extra, "extra-block presence mismatch")
    if expected_has_extra:
        extra_source = evidence_record(extra_source, "extra-block source")
        extra_before = evidence_record(extra_before, "pre-run extra block")
        extra_after = evidence_record(extra_after, "post-run extra block")
        extra_source_path = extra_source.get("path")
        extra_runtime_path = extra_before.get("path")
        require(isinstance(extra_source_path, str), "missing extra-block source path")
        require(isinstance(extra_runtime_path, str), "missing runtime extra-block path")
        expected_extra_path = str(Path(args.extra_block).expanduser().resolve())
        require(extra_source_path == expected_extra_path, "extra-block source path mismatch")
        require(
            extra_after == file_evidence(Path(extra_runtime_path)),
            "post-run extra-block evidence mismatch",
        )
        extra_mode = receipt.get("extra_block_mode")
        require(extra_mode == args.extra_block_mode, "extra-block mode mismatch")
        if extra_mode != "rw" or extra_runtime_path != expected_extra_path:
            require(
                extra_source == file_evidence(Path(expected_extra_path)),
                "extra-block source evidence mismatch",
            )
        else:
            require(
                extra_source == extra_before,
                "in-place writable extra block has inconsistent pre-run evidence",
            )
        if extra_mode != "rw":
            require(extra_before == extra_after, "read-only extra block changed during QEMU")
        extra_block = Drive(Path(extra_runtime_path), extra_mode)

    expected_command = list(
        build_qemu_command(
            arch=args.arch,
            kernel=Path(str(kernel["path"])),
            rootfs=Drive(Path(rootfs_runtime_path), args.rootfs_mode),
            extra_block=extra_block,
            esp=esp,
            ovmf_code=ovmf_code,
            ovmf_vars=ovmf_vars,
            direct_kernel=args.direct_kernel,
            memory=args.memory,
            cpus=args.cpus,
            qemu_binary=qemu_path,
        )
    )
    command = receipt.get("command")
    require(command == expected_command, "QEMU command does not match the recorded inputs")
    log = file_evidence(Path(args.log))
    require(receipt_evidence["log"] == log, "QEMU log evidence mismatch")
    interaction = receipt.get("interaction")
    external_input_producer = (
        isinstance(interaction, dict)
        and interaction.get("external_input_producer") is True
    )
    require(
        external_input_producer == (args.commands is not None),
        "external-producer receipts require --commands and ordinary receipts reject it",
    )
    if external_input_producer:
        validate_completed_input_receipt(receipt, Path(args.commands))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--receipt", required=True)
    parser.add_argument("--arch", choices=("x86_64",), required=True)
    parser.add_argument(
        "--direct-kernel",
        action="store_true",
        help="validate the debug-only direct-kernel boot topology",
    )
    parser.add_argument("--cpus", type=int, required=True)
    parser.add_argument("--kernel", required=True)
    parser.add_argument("--rootfs", required=True)
    parser.add_argument("--esp", help="expected GPT/FAT32 ESP source for UEFI receipts")
    parser.add_argument("--ovmf-code", help="expected OVMF code image for UEFI receipts")
    parser.add_argument("--ovmf-vars", help="expected OVMF vars template for UEFI receipts")
    parser.add_argument(
        "--rootfs-mode",
        choices=("snapshot", "readonly", "rw"),
        required=True,
    )
    parser.add_argument("--extra-block")
    parser.add_argument(
        "--extra-block-mode",
        choices=("snapshot", "readonly", "rw"),
        default="rw",
    )
    parser.add_argument("--log", required=True)
    parser.add_argument("--qemu-binary", required=True)
    parser.add_argument(
        "--commands",
        help="require a complete external-producer stdin receipt for this command stream",
    )
    parser.add_argument("--memory", default="1G")
    args = parser.parse_args()
    try:
        validate(args)
    except (OSError, ReceiptError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    print(f"QEMU receipt: PASS receipt={Path(args.receipt).resolve()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
