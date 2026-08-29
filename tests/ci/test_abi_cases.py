#!/usr/bin/env python3
"""Focused unit checks for the declarative ABI case framework."""

from __future__ import annotations

import copy
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tools" / "abi_cases.py"
SYSCALL_TABLE = ROOT / "docs" / "linux-abi" / "linux-v6.12.103-arch-x86-entry-syscalls-syscall_64.tbl"
SPEC = importlib.util.spec_from_file_location("abi_cases", TOOL)
assert SPEC and SPEC.loader
abi_cases = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = abi_cases
SPEC.loader.exec_module(abi_cases)


class AbiCaseTests(unittest.TestCase):
    def manifest(self) -> dict:
        return json.loads((ROOT / "docs" / "linux-abi" / "abi-cases.json").read_text())

    def init_repo(self, path: Path, filename: str = "tracked") -> str:
        subprocess.run(("git", "init", "-q", str(path)), check=True)
        subprocess.run(("git", "-C", str(path), "config", "user.email", "abi@example.invalid"), check=True)
        subprocess.run(("git", "-C", str(path), "config", "user.name", "ABI tests"), check=True)
        (path / filename).write_text("initial\n")
        subprocess.run(("git", "-C", str(path), "add", filename), check=True)
        subprocess.run(("git", "-C", str(path), "commit", "-qm", "initial"), check=True)
        return subprocess.run(("git", "-C", str(path), "rev-parse", "HEAD"), check=True, capture_output=True, text=True).stdout.strip()

    def three_checkouts(self, temporary: str) -> tuple[Path, dict[str, Path]]:
        parent = Path(temporary)
        root = parent / "thekernel"
        root.mkdir()
        self.init_repo(root, "source.c")
        siblings = {name: parent / path for name, path in (("ax", "thekernel-ax"), ("linux_abi", "thekernel-linux-abi"))}
        refs = {}
        for name, checkout in siblings.items():
            checkout.mkdir()
            refs[name] = self.init_repo(checkout)
        (root / "config").mkdir()
        (root / "config" / "source-combination.toml").write_text(
            "schema = 1\n\n"
            "[source.ax]\nrepository = \"example/ax\"\nref = \"%s\"\npath = \"thekernel-ax\"\n\n"
            "[source.linux_abi]\nrepository = \"example/linux-abi\"\nref = \"%s\"\npath = \"thekernel-linux-abi\"\n" % (refs["ax"], refs["linux_abi"])
        )
        for relative in abi_cases.RECEIPT_CLOSURE_INPUTS:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"fixture {relative}\n")
        contract = root / "docs/linux-abi/contracts/fixture.json"
        contract.parent.mkdir(parents=True, exist_ok=True)
        contract.write_text("{}\n")
        subprocess.run(("git", "-C", str(root), "add", "config/source-combination.toml"), check=True)
        subprocess.run(("git", "-C", str(root), "add", "docs/linux-abi"), check=True)
        subprocess.run(("git", "-C", str(root), "commit", "-qm", "source combination"), check=True)
        return root, siblings

    def native_ni_table_syscalls(self) -> set[str]:
        syscalls: set[str] = set()
        for line in SYSCALL_TABLE.read_text().splitlines():
            fields = line.split()
            if not fields or fields[0].startswith("#"):
                continue
            number, abi, name = fields[:3]
            entry = fields[3] if len(fields) > 3 else "sys_ni_syscall"
            if abi in {"common", "64"} and entry == "sys_ni_syscall":
                syscalls.add(name)
        return syscalls

    def test_checked_in_manifest_is_valid_and_sharding_is_stable(self) -> None:
        cases = abi_cases.load_manifest()
        self.assertEqual(
            [case["id"] for case in cases],
            [
                "eventfd.portable-differential",
                "creat.raw-differential",
                "native-ni.fixed-slots",
            ],
        )
        for case in cases:
            self.assertTrue(abi_cases.is_gate_eligible(case))
            self.assertNotIn("source_combination_id", case["oracle_configs"]["thekernel"])
            self.assertEqual(
                case["resources"],
                {"cpus": 4, "memory_mib": 1024, "profile_timeout_seconds": 300},
            )
            case_id = case["id"]
            assigned = [
                shard for shard in range(23)
                if case in abi_cases.shard_cases(cases, shard, 23)
            ]
            self.assertEqual(
                assigned,
                [int.from_bytes(abi_cases.hashlib.sha256(case_id.encode()).digest(), "big") % 23],
            )

    def test_native_ni_manifest_matches_fixed_sys_ni_table(self) -> None:
        case = next(case for case in abi_cases.load_manifest()
                    if case["id"] == "native-ni.fixed-slots")
        self.assertEqual(len(case["syscalls"]), 17)
        self.assertEqual(set(case["syscalls"]), self.native_ni_table_syscalls())

    def test_linux_product_targets_match_the_fixed_oracle_manifest(self) -> None:
        oracle_document = json.loads(
            (ROOT / "docs/linux-abi/oracle-configs.json").read_text()
        )
        product = next(
            oracle for oracle in oracle_document["oracles"]
            if oracle["id"] == "q35-product"
        )
        expected = {
            "config_id": product["id"],
            "config_sha256": product["configuration"]["final_config_sha256"],
            "kernel_sha256": product["artifact"]["sha256"],
        }
        for case in abi_cases.load_manifest():
            if "linux-product" in case["targets"]:
                self.assertEqual(case["oracle_configs"]["linux-product"], expected)

    def test_native_ni_host_transcript_validates(self) -> None:
        case = next(case for case in abi_cases.load_manifest()
                    if case["id"] == "native-ni.fixed-slots")
        source = ROOT / case["source"][0]
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "native-ni-differential"
            subprocess.run(("cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-o", str(binary), str(source)),
                           check=True)
            completed = subprocess.run((str(binary),), check=True, capture_output=True, text=True)
        results = abi_cases.validate_transcript(completed.stdout, [case])
        self.assertEqual(results, [{"id": "native-ni.fixed-slots", "outcome": "enosys"}])

    def test_functional_host_transcripts_validate(self) -> None:
        cases = {case["id"]: case for case in abi_cases.load_manifest()}
        for identifier in ("eventfd.portable-differential", "creat.raw-differential"):
            case = cases[identifier]
            source = ROOT / case["source"][0]
            with self.subTest(case=identifier), tempfile.TemporaryDirectory() as temporary:
                binary = Path(temporary) / identifier
                subprocess.run(
                    ("cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-o", str(binary), str(source)),
                    check=True,
                )
                completed = subprocess.run(
                    (str(binary.resolve()),), check=True, capture_output=True, text=True
                )
                self.assertEqual(
                    abi_cases.validate_transcript(completed.stdout, [case]),
                    [{"id": identifier, "outcome": "pass"}],
                )

    def test_rejects_duplicate_unsafe_and_invalid_case_fields(self) -> None:
        document = self.manifest()
        case = document["cases"][0]
        document["cases"].append(copy.deepcopy(case))
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "cases.json"
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "duplicate id"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["source"] = ["../escape.c"]
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "unsafe path"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["resources"] = {"cpus": 4, "memory_mib": 1024}
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "profile_timeout_seconds"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["resources"]["cpus"] = 0
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "resources.cpus"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["timeout"] = 0
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "timeout"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["expected"] = "maybe"
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "expected"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["expected"] = "fail"
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "expected"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["oracle_configs"]["linux-product"].pop("kernel_sha256")
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "kernel_sha256"):
                abi_cases.load_manifest(path)

            document = self.manifest()
            document["cases"][0]["oracle_configs"]["linux-product"]["kernel_sha256"] = "0" * 64
            path.write_text(json.dumps(document))
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "differs from checked-in"):
                abi_cases.load_manifest(path)

    def test_selected_resources_require_one_compatible_profile(self) -> None:
        cases = abi_cases.load_manifest()
        self.assertEqual(
            abi_cases.selected_resources(cases),
            {"cpus": 4, "memory_mib": 1024, "profile_timeout_seconds": 300},
        )
        incompatible = copy.deepcopy(cases)
        incompatible[1]["resources"]["memory_mib"] = 2048
        with self.assertRaisesRegex(abi_cases.AbiCaseError, "incompatible"):
            abi_cases.selected_resources(incompatible)

    def test_transcript_requires_one_complete_non_skipped_case(self) -> None:
        case = abi_cases.load_manifest()[0]
        transcript = "\n".join((
            "THEKERNEL_ABI_CASE eventfd.portable-differential",
            "THEKERNEL_ABI_ASSERT eventfd.portable-differential COUNTER pass",
            "THEKERNEL_ABI_ASSERT eventfd.portable-differential EVENTFD2 pass",
            "THEKERNEL_EVENTFD_OK",
            "THEKERNEL_ABI_RESULT eventfd.portable-differential pass",
            "",
        ))
        results = abi_cases.validate_transcript(transcript, [case])
        self.assertEqual(results, [{"id": case["id"], "outcome": "pass"}])
        self.assertEqual(
            abi_cases.ktap_plan(results),
            "KTAP version 1\n1..1\nok 1 - eventfd.portable-differential\n",
        )

        for broken, pattern in (
            (transcript.replace("pass\n", "skip\n", 1), "unallowed skip"),
            (transcript + "THEKERNEL_ABI_RESULT eventfd.portable-differential pass\n", "exactly one result"),
            (transcript.replace("THEKERNEL_EVENTFD_OK\n", ""), "required marker"),
            (transcript + "THEKERNEL_ABI_CASE unknown.case\n", "unknown cases"),
            (transcript.replace("THEKERNEL_EVENTFD_OK", "THEKERNEL_ABI_RESULT eventfd.portable-differential pass\nTHEKERNEL_EVENTFD_OK"), "exactly one result"),
            (transcript.replace("THEKERNEL_ABI_ASSERT eventfd.portable-differential COUNTER pass\n", ""), "one assertion per syscall"),
            (transcript.replace("THEKERNEL_EVENTFD_OK\n", "") + "THEKERNEL_EVENTFD_OK\n", "between case and result"),
            (transcript.replace("COUNTER pass", "COUNTER fail"), "failed assertion"),
        ):
            with self.subTest(pattern=pattern), self.assertRaisesRegex(abi_cases.AbiCaseError, pattern):
                abi_cases.validate_transcript(broken, [case])

    def test_receipt_binds_case_inputs_and_file_content(self) -> None:
        case = copy.deepcopy(abi_cases.load_manifest()[0])
        case["source"] = ["source.c"]
        case["binary"] = "bin/case"
        case["targets"] = ["linux-product"]
        case["oracle_configs"] = {"linux-product": case["oracle_configs"]["linux-product"]}
        with tempfile.TemporaryDirectory() as temporary:
            root, _ = self.three_checkouts(temporary)
            (root / "bin").mkdir()
            (root / "bin" / "case").write_text("binary")
            subprocess.run(("git", "-C", str(root), "add", "bin/case"), check=True)
            subprocess.run(("git", "-C", str(root), "commit", "-qm", "binary"), check=True)
            command = ["bin/case", "--one"]
            transcript = "\n".join((
                "THEKERNEL_ABI_CASE eventfd.portable-differential",
                "THEKERNEL_ABI_ASSERT eventfd.portable-differential LEGACY pass",
                "THEKERNEL_ABI_ASSERT eventfd.portable-differential EVENTFD2 pass",
                "THEKERNEL_EVENTFD_OK",
                "THEKERNEL_ABI_RESULT eventfd.portable-differential pass",
                "",
            ))
            receipt = abi_cases.build_receipt(
                case, repo_root=root, command=command, target="linux-product", exit_code=0,
                transcript=transcript,
            )
            self.assertEqual(receipt["command"], command)
            self.assertIn("source_identity", receipt)
            self.assertNotIn("source_hashes", receipt)
            abi_cases.verify_receipt(
                receipt, case, repo_root=root, command=command, target="linux-product", exit_code=0,
                transcript=transcript,
            )
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "exit code zero"):
                abi_cases.verify_receipt(
                    receipt, case, repo_root=root, command=command, target="linux-product", exit_code=1,
                    transcript=transcript,
                )
            (root / "bin" / "case").write_text("changed binary")
            subprocess.run(("git", "-C", str(root), "add", "bin/case"), check=True)
            subprocess.run(("git", "-C", str(root), "commit", "-qm", "changed binary"), check=True)
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "does not bind"):
                abi_cases.verify_receipt(
                    receipt, case, repo_root=root, command=command, target="linux-product", exit_code=0,
                    transcript=transcript,
                )

    def test_runtime_source_identity_is_dynamic_clean_and_declared(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root, siblings = self.three_checkouts(temporary)
            initial = abi_cases.capture_source_identity(root)
            (root / "source.c").write_text("next\n")
            subprocess.run(("git", "-C", str(root), "add", "source.c"), check=True)
            subprocess.run(("git", "-C", str(root), "commit", "-qm", "next head"), check=True)
            advanced = abi_cases.capture_source_identity(root)
            self.assertNotEqual(initial["sources"]["thekernel"]["commit"], advanced["sources"]["thekernel"]["commit"])
            self.assertNotEqual(initial["combination_id"], advanced["combination_id"])
            for name, checkout in {"thekernel": root, **siblings}.items():
                (checkout / "dirty").write_text("dirty\n")
                with self.subTest(source=name), self.assertRaisesRegex(abi_cases.AbiCaseError, f"{name}.*clean"):
                    abi_cases.capture_source_identity(root)
                (checkout / "dirty").unlink()
            (siblings["ax"] / "tracked").write_text("different\n")
            subprocess.run(("git", "-C", str(siblings["ax"]), "add", "tracked"), check=True)
            subprocess.run(("git", "-C", str(siblings["ax"]), "commit", "-qm", "mismatch"), check=True)
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "ax.*differs"):
                abi_cases.capture_source_identity(root)

    def test_runtime_source_identity_rejects_non_top_level_and_bad_sibling_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root, _ = self.three_checkouts(temporary)
            nested = root / "nested"
            nested.mkdir()
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "top-level"):
                abi_cases.capture_source_identity(nested)
            config = root / "config" / "source-combination.toml"
            config.write_text(config.read_text().replace('path = "thekernel-ax"', 'path = "missing"'))
            subprocess.run(("git", "-C", str(root), "add", "config/source-combination.toml"), check=True)
            subprocess.run(("git", "-C", str(root), "commit", "-qm", "bad path"), check=True)
            with self.assertRaisesRegex(abi_cases.AbiCaseError, "cannot inspect checkout"):
                abi_cases.capture_source_identity(root)

    def test_enosys_and_skip_semantics(self) -> None:
        case = copy.deepcopy(abi_cases.load_manifest()[0])
        case["expected"] = "enosys"
        transcript = "\n".join((
            "THEKERNEL_ABI_CASE eventfd.portable-differential",
            "THEKERNEL_ABI_ASSERT eventfd.portable-differential PROBE enosys",
            "THEKERNEL_ABI_ASSERT eventfd.portable-differential PROBE2 enosys",
            "THEKERNEL_EVENTFD_OK",
            "THEKERNEL_ABI_RESULT eventfd.portable-differential enosys",
        ))
        results = abi_cases.validate_transcript(transcript, [case])
        self.assertEqual(abi_cases.ktap_plan(results), "KTAP version 1\n1..1\nok 1 - eventfd.portable-differential # ENOSYS expected\n")
        self.assertTrue(abi_cases.is_gate_eligible(case))
        case["expected"] = "skip-permitted"
        self.assertFalse(abi_cases.is_gate_eligible(case))
        with self.assertRaisesRegex(abi_cases.AbiCaseError, "expected skip"):
            abi_cases.validate_transcript(transcript, [case])


if __name__ == "__main__":
    unittest.main()
