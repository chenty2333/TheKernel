#!/usr/bin/env python3
"""Mocked checks for the q35 ABI runner's publication boundary."""
from __future__ import annotations

import importlib.util
import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("abi_runner", ROOT / "tools" / "abi_runner.py")
assert SPEC and SPEC.loader
abi_runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(abi_runner)


CASE = {
    "id": "eventfd.portable-differential", "binary": ".state/abi-binaries/eventfd-differential",
    "syscalls": ["eventfd"],
    "targets": ["linux-product", "thekernel"], "expected": "pass",
    "timeout": 30,
    "resources": {"cpus": 4, "memory_mib": 1024, "profile_timeout_seconds": 300},
    "required_markers": ["THEKERNEL_EVENTFD_OK"], "oracle_configs": {"linux-product": {}, "thekernel": {}},
}
IDENTITY = {"schema": 1, "combination_id": "source-combination-v1-" + "a" * 64,
            "sources": {name: {"commit": "a" * 40, "tree": "b" * 40, "clean": True}
                        for name in ("thekernel", "ax", "linux_abi")}}


class AbiRunnerTests(unittest.TestCase):
    UAPI_PROVENANCE = {
        "headers_path": "/pinned/uapi", "headers_tree_sha256": "e" * 64,
        "published_metadata_path": "/published/.uapi-sha256", "published_metadata_sha256": "f" * 64,
        "rootfs_metadata_path": "/usr/share/thekernel/abi-uapi-sha256",
        "rootfs_metadata_sha256": "1" * 64,
    }

    def launch_receipt(self, root: Path) -> Path:
        log = root / "system-test.log"
        log.write_text("KTAP version 1\n1..1\nok 1 - smoke\n# THEKERNEL_SYSTEM_TEST_COMPLETE\n")
        digest = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
        receipt = root / "system-test-receipt.json"
        receipt.write_text(json.dumps({
            "state": "recorded", "returncode": 0, "timed_out": False,
            "interrupted": False, "runner_terminated": False,
            "guest_clean_shutdown": True, "error_message": None,
            "direct_kernel": False,
            "interaction": {"interactive": True,
                            "input_after_marker": "# THEKERNEL_SYSTEM_TEST_COMPLETE",
                            "stop_after_marker": None},
            "source_identity": {
                "schema": 1, "combination_id": IDENTITY["combination_id"],
                "sources": {
                    name: {"commit": source["commit"], "tree": source["tree"],
                           "worktree_dirty": False, "match_declared": True}
                    for name, source in IDENTITY["sources"].items()
                },
            },
            "kernel": {"path": str((root / "kernel").resolve()), "sha256": digest(root / "kernel")},
            "esp_source": {"path": str((root / "esp").resolve()), "sha256": digest(root / "esp")},
            "rootfs_source": {"path": str((root / "rootfs").resolve()), "sha256": digest(root / "rootfs")},
            "log": {"path": str(log.resolve()), "sha256": digest(log)},
        }))
        return receipt

    def arguments(self, root: Path):
        for name in ("rootfs", "linux", "kernel", "esp"):
            (root / name).write_bytes(name.encode())
        CASE["oracle_configs"]["linux-product"] = {
            "kernel_sha256": hashlib.sha256((root / "linux").read_bytes()).hexdigest()
        }
        uapi_headers = root / "uapi"
        uapi_headers.mkdir(exist_ok=True)
        return SimpleNamespace(repo_root=root, rootfs=root / "rootfs", linux_kernel=root / "linux",
            thekernel_kernel=root / "kernel", thekernel_esp=root / "esp", output=root / "published",
            thekernel_launch_receipt=self.launch_receipt(root), uapi_headers=uapi_headers,
            case=None, qemu="qemu", accel="tcg")

    def test_rootfs_binary_verification_compares_installed_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            rootfs = root / "rootfs"
            rootfs.write_bytes(b"ext4")
            published = root / CASE["binary"]
            published.parent.mkdir(parents=True)
            published.write_bytes(b"same-binary")

            def fake_debugfs(command, **_kwargs):
                destination = Path(command[2].rsplit(" ", 1)[1])
                destination.write_bytes(b"same-binary")
                return SimpleNamespace(returncode=0, stdout="", stderr="")

            with patch.object(abi_runner.shutil, "which", return_value="/usr/bin/debugfs"), \
                 patch.object(abi_runner.subprocess, "run", side_effect=fake_debugfs):
                hashes = abi_runner._verify_rootfs_case_binaries(rootfs, [CASE], root)
            self.assertEqual(hashes[CASE["id"]], hashlib.sha256(b"same-binary").hexdigest())

            def mismatched_debugfs(command, **_kwargs):
                destination = Path(command[2].rsplit(" ", 1)[1])
                destination.write_bytes(b"different")
                return SimpleNamespace(returncode=0, stdout="", stderr="")

            with patch.object(abi_runner.shutil, "which", return_value="/usr/bin/debugfs"), \
                 patch.object(abi_runner.subprocess, "run", side_effect=mismatched_debugfs), \
                 self.assertRaisesRegex(abi_runner.AbiRunnerError, "differs from published"):
                abi_runner._verify_rootfs_case_binaries(rootfs, [CASE], root)

    def test_builds_direct_linux_q35_and_publishes_only_after_both_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = self.arguments(root)
            configs = []
            receipt_transcripts = []
            def fake_run(config):
                configs.append(config)
                config.log_path.parent.mkdir(parents=True, exist_ok=True)
                if config.direct_kernel:
                    log = "THEKERNEL_ABI_CASE eventfd.portable-differential\nTHEKERNEL_ABI_ASSERT eventfd.portable-differential COUNTER pass\nTHEKERNEL_EVENTFD_OK\nTHEKERNEL_ABI_RESULT eventfd.portable-differential pass\nTHEKERNEL_ABI_INIT_COMPLETE\n"
                else:
                    log = "# eventfd: THEKERNEL_ABI_CASE eventfd.portable-differential\n# eventfd: THEKERNEL_ABI_ASSERT eventfd.portable-differential COUNTER pass\n# eventfd: THEKERNEL_EVENTFD_OK\n# eventfd: THEKERNEL_ABI_RESULT eventfd.portable-differential pass\n# THEKERNEL_SYSTEM_TEST_COMPLETE\n"
                config.log_path.write_text(log)
                config.receipt_path.write_text(json.dumps({
                    "returncode": 0,
                    "guest_clean_shutdown": True,
                }))
                return SimpleNamespace(command=("qemu", "-machine", "q35"), returncode=0,
                    error_message=None, timed_out=False, interrupted=False, runner_terminated=False,
                    guest_clean_shutdown=True, log_path=config.log_path)
            def receipt(case, **kwargs):
                receipt_transcripts.append(kwargs["transcript"])
                return {"case_id": case["id"], "target": kwargs["target"]}
            with patch.object(abi_runner.abi_cases, "capture_source_identity", return_value=IDENTITY), \
                 patch.object(abi_runner.abi_cases, "load_manifest", return_value=[CASE]), \
                 patch.object(abi_runner.abi_cases, "build_receipt", side_effect=receipt), \
                 patch.object(abi_runner, "_verify_rootfs_case_binaries", return_value={CASE["id"]: "d" * 64}), \
                 patch.object(abi_runner, "_verify_uapi_provenance", return_value=self.UAPI_PROVENANCE), \
                 patch.object(abi_runner, "run", side_effect=fake_run):
                result = abi_runner.execute(args)
            self.assertEqual(result, args.output)
            self.assertTrue((result / "run-group.json").is_file())
            self.assertTrue((result / "receipts/linux-product/eventfd.portable-differential.json").is_file())
            self.assertTrue(configs[0].direct_kernel)
            self.assertIsNone(configs[0].esp)
            self.assertIn("-append", configs[0].extra_args)
            self.assertIn("thekernel_abi_cases=eventfd.portable-differential", configs[0].extra_args[-1])
            self.assertFalse(configs[1].direct_kernel)
            self.assertEqual(configs[1].esp, args.thekernel_esp)
            self.assertEqual(configs[0].cpus, 4)
            self.assertEqual(configs[0].memory, "1024M")
            self.assertEqual(configs[0].limits.total_timeout_secs, 300)
            self.assertEqual(len(receipt_transcripts), 2)
            self.assertTrue(all("ABI_INIT_COMPLETE" not in item for item in receipt_transcripts))
            published = json.loads((result / "receipts/linux-product/eventfd.portable-differential.json").read_text())
            self.assertEqual(published["runner"]["thekernel_launch_receipt_sha256"],
                             hashlib.sha256(args.thekernel_launch_receipt.read_bytes()).hexdigest())
            self.assertEqual(published["runner"]["uapi"], self.UAPI_PROVENANCE)

    def test_failure_never_publishes_run_group(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = self.arguments(root)
            def failed(config):
                config.log_path.parent.mkdir(parents=True, exist_ok=True)
                config.log_path.write_text("panic\n")
                return SimpleNamespace(command=("qemu",), returncode=124, error_message="timeout",
                    timed_out=True, interrupted=False, runner_terminated=False, guest_clean_shutdown=False,
                    log_path=config.log_path)
            with patch.object(abi_runner.abi_cases, "capture_source_identity", return_value=IDENTITY), \
                 patch.object(abi_runner.abi_cases, "load_manifest", return_value=[CASE]), \
                 patch.object(abi_runner, "_verify_rootfs_case_binaries", return_value={CASE["id"]: "d" * 64}), \
                 patch.object(abi_runner, "_verify_uapi_provenance", return_value=self.UAPI_PROVENANCE), \
                 patch.object(abi_runner, "run", side_effect=failed):
                with self.assertRaisesRegex(abi_runner.AbiRunnerError, "non-clean"):
                    abi_runner.execute(args)
            self.assertFalse(args.output.exists())

    def test_launch_receipt_rejects_stale_artifacts_and_incomplete_transcript(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = self.arguments(root)
            receipt = json.loads(args.thekernel_launch_receipt.read_text())
            receipt["kernel"]["sha256"] = "0" * 64
            args.thekernel_launch_receipt.write_text(json.dumps(receipt))
            with self.assertRaisesRegex(abi_runner.AbiRunnerError, "kernel hash"):
                abi_runner._validate_thekernel_launch_receipt(
                    args.thekernel_launch_receipt, identity=IDENTITY,
                    kernel=args.thekernel_kernel, esp=args.thekernel_esp, rootfs=args.rootfs,
                )

            args = self.arguments(root)
            log = Path(json.loads(args.thekernel_launch_receipt.read_text())["log"]["path"])
            log.write_text("KTAP version 1\n1..1\nok 1 - smoke\n")
            receipt = json.loads(args.thekernel_launch_receipt.read_text())
            receipt["log"]["sha256"] = hashlib.sha256(log.read_bytes()).hexdigest()
            args.thekernel_launch_receipt.write_text(json.dumps(receipt))
            with self.assertRaisesRegex(abi_runner.AbiRunnerError, "incomplete"):
                abi_runner._validate_thekernel_launch_receipt(
                    args.thekernel_launch_receipt, identity=IDENTITY,
                    kernel=args.thekernel_kernel, esp=args.thekernel_esp, rootfs=args.rootfs,
                )

            args = self.arguments(root)
            log = Path(json.loads(args.thekernel_launch_receipt.read_text())["log"]["path"])
            log.write_text(
                "KTAP version 1\n1..2\nok 1 - smoke\n# THEKERNEL_SYSTEM_TEST_COMPLETE\n"
            )
            receipt = json.loads(args.thekernel_launch_receipt.read_text())
            receipt["log"]["sha256"] = hashlib.sha256(log.read_bytes()).hexdigest()
            args.thekernel_launch_receipt.write_text(json.dumps(receipt))
            with self.assertRaisesRegex(abi_runner.AbiRunnerError, "incomplete or invalid"):
                abi_runner._validate_thekernel_launch_receipt(
                    args.thekernel_launch_receipt, identity=IDENTITY,
                    kernel=args.thekernel_kernel, esp=args.thekernel_esp, rootfs=args.rootfs,
                )

    def test_uapi_provenance_rejects_unbound_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            headers = root / ".state/uapi"
            headers.mkdir(parents=True)
            rootfs = root / "rootfs"
            rootfs.write_bytes(b"ext4")
            metadata = root / CASE["binary"]
            metadata.parent.mkdir(parents=True)
            (metadata.parent / ".uapi-sha256").write_text("a" * 64 + "\n")

            manifest = {"headers": {"materialized_path": ".state/uapi", "tree_sha256": "a" * 64}}
            def extract(_rootfs, _guest, destination):
                destination.write_text("a" * 64 + "\n")
            with patch.object(abi_runner.abi_uapi, "load_manifest", return_value=manifest), \
                 patch.object(abi_runner.abi_uapi, "tree_sha256", return_value="a" * 64), \
                 patch.object(abi_runner, "_extract_rootfs_file", side_effect=extract):
                evidence = abi_runner._verify_uapi_provenance(
                    repo_root=root, uapi_headers=headers, rootfs=rootfs, cases=[CASE]
                )
            self.assertEqual(evidence["headers_tree_sha256"], "a" * 64)

            (metadata.parent / ".uapi-sha256").write_text("b" * 64 + "\n")
            with patch.object(abi_runner.abi_uapi, "load_manifest", return_value=manifest), \
                 patch.object(abi_runner.abi_uapi, "tree_sha256", return_value="a" * 64), \
                 self.assertRaisesRegex(abi_runner.AbiRunnerError, "published ABI UAPI metadata"):
                abi_runner._verify_uapi_provenance(
                    repo_root=root, uapi_headers=headers, rootfs=rootfs, cases=[CASE]
                )

            (metadata.parent / ".uapi-sha256").write_text("a" * 64 + "\n")
            def mismatched_rootfs(_rootfs, _guest, destination):
                destination.write_text("b" * 64 + "\n")
            with patch.object(abi_runner.abi_uapi, "load_manifest", return_value=manifest), \
                 patch.object(abi_runner.abi_uapi, "tree_sha256", return_value="a" * 64), \
                 patch.object(abi_runner, "_extract_rootfs_file", side_effect=mismatched_rootfs), \
                 self.assertRaisesRegex(abi_runner.AbiRunnerError, "rootfs ABI UAPI metadata"):
                abi_runner._verify_uapi_provenance(
                    repo_root=root, uapi_headers=headers, rootfs=rootfs, cases=[CASE]
                )

    def test_source_drift_never_publishes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            args = self.arguments(root)
            identities = [IDENTITY, {**IDENTITY, "combination_id": "source-combination-v1-" + "c" * 64}]
            def fake_target(**_kwargs):
                return [{"case_id": "eventfd.portable-differential", "target": "linux-product"}]
            with patch.object(abi_runner.abi_cases, "capture_source_identity", side_effect=identities), \
                 patch.object(abi_runner.abi_cases, "load_manifest", return_value=[CASE]), \
                 patch.object(abi_runner, "_verify_rootfs_case_binaries", return_value={CASE["id"]: "d" * 64}), \
                 patch.object(abi_runner, "_verify_uapi_provenance", return_value=self.UAPI_PROVENANCE), \
                 patch.object(abi_runner, "_run_target", side_effect=fake_target):
                with self.assertRaisesRegex(abi_runner.AbiRunnerError, "drifted"):
                    abi_runner.execute(args)
            self.assertFalse(args.output.exists())


if __name__ == "__main__":
    unittest.main()
