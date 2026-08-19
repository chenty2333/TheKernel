from __future__ import annotations

import os
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT))

from tools.kvm_scheduler_baseline import (
    DEFAULT_LINUX_CMDLINE,
    PIN_REPORT_LEGACY_SCHEMA,
    PIN_REPORT_SCHEMA,
    RAW_COLUMNS,
    Sample,
    _target_images,
    _read_raw,
    build_parser,
    nearest_rank,
    parse_guest_log,
    pin_report_failure_status,
    scheduler_sample_checksum,
    stats,
    validate_pin_report,
)
from tools.qemu_runner.command import build_qemu_command
from tools.qemu_runner.model import Drive


SOURCE = REPO_ROOT / "tests" / "guest" / "tools" / "scheduler-baseline.c"


class SchedulerBaselineTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        compiler = shutil.which("cc")
        if compiler is None:
            raise unittest.SkipTest("host C compiler is unavailable")
        cls.temporary = tempfile.TemporaryDirectory()
        cls.binary = Path(cls.temporary.name) / "scheduler-baseline"
        build = subprocess.run(
            [compiler, "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread", str(SOURCE), "-o", str(cls.binary)],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        if build.returncode != 0:
            raise AssertionError(build.stderr)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.temporary.cleanup()

    def run_helper(self, workload: str, placement: str) -> subprocess.CompletedProcess[str]:
        cpus = "2" if placement == "cross" else "1"
        allowed = os.sched_getaffinity(0)
        if 0 not in allowed or (placement == "cross" and 1 not in allowed):
            self.skipTest("host affinity does not include guest helper CPUs 0 and 1")
        if placement == "cross" and len(os.sched_getaffinity(0)) < 2:
            self.skipTest("host affinity exposes fewer than two CPUs")
        return subprocess.run(
            [str(self.binary), "--workload", workload, "--placement", placement,
             "--iterations", "8", "--warmup", "2", "--cpus", cpus],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )

    @staticmethod
    def write_pin_report(
        path: Path,
        *,
        vcpu_cpus: tuple[int, ...],
        io_cpus: tuple[int, ...],
        count: int = 1,
        status: str = "ok",
        housekeeping_cpus: tuple[int, ...] | None = None,
    ) -> None:
        if housekeeping_cpus is None:
            housekeeping_cpus = (max(vcpu_cpus + io_cpus) + 1,)
        measurement = sorted(set(vcpu_cpus) | set(io_cpus))
        path.write_text(json.dumps({
            "schema": PIN_REPORT_SCHEMA,
            "pid": 1234,
            "expected_vcpu_count": count,
            "requested_vcpu_cpus": list(vcpu_cpus),
            "requested_io_cpus": list(io_cpus),
            "requested_backend_cpus": [],
            "housekeeping_cpus": list(housekeeping_cpus),
            "measurement_cpus": measurement,
            "measurement_smt_siblings": measurement,
            "vcpu_threads": {
                str(index): {
                    "tid": 2000 + index,
                    "name": f"CPU {index}/KVM",
                    "affinity": [vcpu_cpus[index % len(vcpu_cpus)]],
                    "tgid": 1234,
                }
                for index in range(count)
            },
            "io_threads": [{
                "tid": 3000, "name": "IO thread", "affinity": list(io_cpus),
                "tgid": 1234,
            }],
            "backend_threads": [],
            "external_processes": [],
            "declared_external_backends": [],
            "qemu_main": {"tid": 1234, "name": "qemu-system-x86_64", "affinity": list(housekeeping_cpus)},
            "unknown_threads": [{"tid": 4000, "name": "unknown", "affinity": list(housekeeping_cpus)}],
            "vcpu_status": status,
            "io_status": status,
            "backend_status": "not_requested",
            "qemu_main_status": "ok",
            "housekeeping_status": "ok",
            "unknown_status": "ok",
            "unknown_off_measurement": True,
            "launcher_affinity": list(housekeeping_cpus),
            "process_inherited_housekeeping": True,
            "new_threads_inherit_housekeeping": True,
            "ptrace_clone_events": True,
            "clone_event_count": 1,
            "unknown_thread_proof": "ptrace-clone-event",
            "exit_readback_tids": sorted(
                [1234, 3000, 4000] + [2000 + index for index in range(count)]
            ),
            "exit_readback_proof": True,
        }) + "\n", encoding="utf-8")

    def test_pin_report_requires_complete_vcpu_set(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0, 1), io_cpus=(2,), count=2)
            payload = json.loads(path.read_text(encoding="utf-8"))
            del payload["vcpu_threads"]["1"]
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("vCPU indices mismatch", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=2,
                requested_vcpu=(0, 1), requested_io=(2,),
            ) or "")

    def test_pin_report_rejects_readback_affinity_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["vcpu_threads"]["0"]["affinity"] = [1]
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("affinity mismatch", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

    def test_pin_report_rejects_missing_and_bad_schema(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.assertIn("invalid pin report", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")
            path.write_text("{}\n", encoding="utf-8")
            self.assertIn("invalid pin report fields", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

    def test_pin_report_accepts_complete_readback(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0, 1), io_cpus=(2,), count=2)
            self.assertIsNone(validate_pin_report(
                path, expected_pid=1234, expected_vcpu_count=2,
                requested_vcpu=(0, 1), requested_io=(2,),
            ))

    def test_pin_report_requires_final_exit_affinity_readback(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["exit_readback_proof"] = False
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("final affinity readback", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")
            payload["exit_readback_proof"] = True
            payload["exit_readback_tids"].remove(3000)
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("without final affinity", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

    def test_unrequested_backend_must_remain_housekeeping_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["backend_threads"] = [
                {"tid": 5000, "name": "vhost-test", "affinity": [0], "tgid": 1234}
            ]
            payload["exit_readback_tids"].append(5000)
            payload["exit_readback_tids"].sort()
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("backend thread affinity mismatch", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

    def test_vcpu_io_records_require_qemu_tgid_and_reject_external_ids(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["vcpu_threads"]["0"]["tgid"] = 9999
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("QEMU thread group", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

            payload["vcpu_threads"]["0"]["tgid"] = 1234
            payload["vcpu_threads"]["0"]["tid"] = 2000
            payload["external_processes"] = [{
                "pid": 2000, "main_tid": 2000, "name": "external",
                "affinity": [2], "tgid": 2000, "exe": "/bin/true",
                "starttime": 1, "backend_authorized": False,
            }]
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("external process PID collides", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

    def test_external_process_pid_cannot_alias_vcpu_io_or_unknown_role(self) -> None:
        cases = (
            ("vcpu", 2000, [0]),
            ("io", 3000, [1]),
            ("unknown", 4000, [2]),
        )
        for role, pid, affinity in cases:
            with self.subTest(role=role):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "thread-pinning.json"
                    self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
                    payload = json.loads(path.read_text(encoding="utf-8"))
                    payload["external_processes"] = [{
                        "pid": pid,
                        "main_tid": pid,
                        "name": "renamed-helper",
                        "affinity": affinity,
                        "tgid": pid,
                        "exe": "/bin/true",
                        "starttime": 200 + pid,
                        "backend_authorized": False,
                    }]
                    path.write_text(json.dumps(payload), encoding="utf-8")
                    reason = validate_pin_report(
                        path,
                        expected_pid=None,
                        expected_vcpu_count=1,
                        requested_vcpu=(0,),
                        requested_io=(1,),
                    )
                    self.assertIn("external process PID collides", reason or "")
                    self.assertEqual(pin_report_failure_status(path, reason), "pinning-error")

    def test_external_backend_requires_exact_runner_identity(self) -> None:
        identity = {
            "pid": 5000,
            "tgid": 5000,
            "exe": "/usr/bin/passt",
            "starttime": 99,
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["backend_threads"] = [{
                "tid": 5000, "tgid": 5000, "name": "vhost-evil", "affinity": [2],
            }]
            payload["external_processes"] = [{
                **identity,
                "main_tid": 5000,
                "name": "vhost-evil",
                "affinity": [2],
                "backend_authorized": True,
            }]
            payload["exit_readback_tids"].append(5000)
            payload["exit_readback_tids"].sort()
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("backend-authorized", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")
            payload["declared_external_backends"] = [identity]
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIsNone(validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
                expected_external_backends=(identity,),
            ))

    def test_external_backend_and_qemu_main_tids_cannot_be_reused(self) -> None:
        identity = {
            "pid": 5000,
            "tgid": 5000,
            "exe": "/usr/bin/passt",
            "starttime": 199,
        }
        for authorized in (False, True):
            with self.subTest(alias="internal-backend", authorized=authorized):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "thread-pinning.json"
                    self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
                    payload = json.loads(path.read_text(encoding="utf-8"))
                    payload["backend_threads"] = [{
                        "tid": 5000,
                        "name": "internal-backend",
                        "affinity": [2],
                        "tgid": 1234,
                    }]
                    payload["external_processes"] = [{
                        **identity,
                        "main_tid": 5000,
                        "name": "renamed-helper",
                        "affinity": [2],
                        "backend_authorized": authorized,
                    }]
                    payload["declared_external_backends"] = [identity] if authorized else []
                    payload["exit_readback_tids"].append(5000)
                    payload["exit_readback_tids"].sort()
                    path.write_text(json.dumps(payload), encoding="utf-8")
                    reason = validate_pin_report(
                        path,
                        expected_pid=None,
                        expected_vcpu_count=1,
                        requested_vcpu=(0,),
                        requested_io=(1,),
                        expected_external_backends=(identity,) if authorized else (),
                    )
                    self.assertIn("external process PID collides", reason or "")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["qemu_main"]["tid"] = 1000
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path,
                expected_pid=None,
                expected_vcpu_count=1,
                requested_vcpu=(0,),
                requested_io=(1,),
            )
            self.assertIn("QEMU main TID does not match report pid", reason or "")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["qemu_main"]["tid"] = 1000
            payload["external_processes"] = [{
                "pid": 1000,
                "main_tid": 1000,
                "name": "renamed-helper",
                "affinity": [2],
                "tgid": 1000,
                "exe": "/bin/true",
                "starttime": 200,
                "backend_authorized": False,
            }]
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path,
                expected_pid=None,
                expected_vcpu_count=1,
                requested_vcpu=(0,),
                requested_io=(1,),
            )
            self.assertIn("external process PID collides", reason or "")

    def test_external_backend_declaration_without_observed_process_is_rejected(self) -> None:
        identity = {
            "pid": 5001,
            "tgid": 5001,
            "exe": "/usr/bin/passt",
            "starttime": 100,
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["declared_external_backends"] = [identity]
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
                expected_external_backends=(identity,),
            )
            self.assertIn("not observed", reason or "")

    def test_external_process_conflicting_same_pid_is_rejected(self) -> None:
        first = {
            "pid": 5002,
            "tgid": 5002,
            "exe": "/usr/bin/passt",
            "starttime": 101,
        }
        second = {**first, "starttime": 102}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["external_processes"] = [
                {
                    **first,
                    "main_tid": 5002,
                    "name": "helper",
                    "affinity": [2],
                    "backend_authorized": True,
                },
                {
                    **second,
                    "main_tid": 5002,
                    "name": "helper-reused",
                    "affinity": [2],
                    "backend_authorized": True,
                },
            ]
            payload["backend_threads"] = [{
                "tid": 5002,
                "name": "helper",
                "affinity": [2],
                "tgid": 5002,
            }]
            payload["declared_external_backends"] = [first]
            payload["exit_readback_tids"].append(5002)
            payload["exit_readback_tids"].sort()
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
                expected_external_backends=(first,),
            )
            self.assertIn("duplicate external process PID", reason or "")
            self.assertEqual(pin_report_failure_status(path, reason), "pinning-error")

    def test_contradictory_clone_proof_is_pinning_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["ptrace_clone_events"] = True
            payload["clone_event_count"] = 0
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path,
                expected_pid=None,
                expected_vcpu_count=1,
                requested_vcpu=(0,),
                requested_io=(1,),
            )
            self.assertIn("invalid clone-event count", reason or "")
            self.assertEqual(pin_report_failure_status(path, reason), "pinning-error")

    def test_external_identity_with_nul_executable_is_rejected_without_exception(self) -> None:
        identity = {
            "pid": 5003,
            "tgid": 5003,
            "exe": "/tmp/\u0000evil",
            "starttime": 103,
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["declared_external_backends"] = [identity]
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            )
            self.assertIn("declared external backend identity", reason or "")

    def test_pin_report_role_containers_fail_closed_without_exceptions(self) -> None:
        role_values = {
            "vcpu_threads": [],
            "io_threads": {},
            "backend_threads": {},
            "unknown_threads": {},
            "external_processes": {},
            "qemu_main": [],
        }
        for field, malformed in role_values.items():
            with self.subTest(field=field):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "thread-pinning.json"
                    self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
                    payload = json.loads(path.read_text(encoding="utf-8"))
                    payload[field] = malformed
                    path.write_text(json.dumps(payload), encoding="utf-8")
                    reason = validate_pin_report(
                        path, expected_pid=None, expected_vcpu_count=1,
                        requested_vcpu=(0,), requested_io=(1,),
                    )
                self.assertIsInstance(reason, str)
                self.assertIn("invalid pin report", reason)

    def test_v3_requires_launcher_inheritance_proof(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            del payload["launcher_affinity"]
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("invalid pin report fields", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

    def test_housekeeping_capability_gaps_are_unsupported(self) -> None:
        for field in (
            "process_inherited_housekeeping",
            "new_threads_inherit_housekeeping",
        ):
            with self.subTest(field=field):
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "thread-pinning.json"
                    self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
                    payload = json.loads(path.read_text(encoding="utf-8"))
                    payload[field] = False
                    path.write_text(json.dumps(payload), encoding="utf-8")
                    reason = validate_pin_report(
                        path,
                        expected_pid=None,
                        expected_vcpu_count=1,
                        requested_vcpu=(0,),
                        requested_io=(1,),
                    )
                    self.assertIn("did not prove", reason or "")
                    self.assertEqual(pin_report_failure_status(path, reason), "unsupported")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["housekeeping_status"] = "not_reported"
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path,
                expected_pid=None,
                expected_vcpu_count=1,
                requested_vcpu=(0,),
                requested_io=(1,),
            )
            self.assertIn("housekeeping pinning status", reason or "")
            self.assertEqual(pin_report_failure_status(path, reason), "unsupported")

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["process_inherited_housekeeping"] = None
            path.write_text(json.dumps(payload), encoding="utf-8")
            reason = validate_pin_report(
                path,
                expected_pid=None,
                expected_vcpu_count=1,
                requested_vcpu=(0,),
                requested_io=(1,),
            )
            self.assertEqual(pin_report_failure_status(path, reason), "pinning-error")

    def test_v3_requires_unknown_threads_to_stay_on_housekeeping(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["unknown_threads"][0]["affinity"] = [9]
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIn("housekeeping-only", validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ) or "")

    def test_legacy_schema_is_the_only_legacy_shape(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "thread-pinning.json"
            self.write_pin_report(path, vcpu_cpus=(0,), io_cpus=(1,))
            payload = json.loads(path.read_text(encoding="utf-8"))
            payload["schema"] = PIN_REPORT_LEGACY_SCHEMA
            for key in (
                "requested_backend_cpus", "housekeeping_cpus", "measurement_cpus",
                "measurement_smt_siblings",
                "backend_threads", "external_processes", "declared_external_backends",
                "qemu_main", "unknown_threads", "backend_status",
                "qemu_main_status", "housekeeping_status", "unknown_status",
                "unknown_off_measurement", "launcher_affinity",
                "process_inherited_housekeeping", "new_threads_inherit_housekeeping",
                "ptrace_clone_events", "clone_event_count", "unknown_thread_proof",
                "exit_readback_tids", "exit_readback_proof",
            ):
                payload.pop(key)
            for record in payload["vcpu_threads"].values():
                record.pop("name")
                record.pop("tgid")
            for record in payload["io_threads"]:
                record.pop("name")
                record.pop("tgid")
            path.write_text(json.dumps(payload), encoding="utf-8")
            self.assertIsNone(validate_pin_report(
                path, expected_pid=None, expected_vcpu_count=1,
                requested_vcpu=(0,), requested_io=(1,),
            ))

    def run_mocked_baseline(
        self,
        *,
        returncode: int,
        complete_guest: bool,
        complete_pin: bool = True,
    ) -> tuple[dict, str, dict]:
        """Run one mocked lane and return manifest, raw TSV, and summary JSON."""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "bzImage"
            rootfs = root / "rootfs.img"
            kernel.write_bytes(b"linux")
            rootfs.write_bytes(b"ext4")
            output = root / "output"
            allowed = sorted(os.sched_getaffinity(0))
            if len(allowed) < 2:
                self.skipTest("host exposes fewer than two CPUs")
            args = build_parser().parse_args([
                "run", "--target", "linux", "--linux-kernel", str(kernel),
                "--linux-rootfs", str(rootfs), "--output", str(output),
                "--workloads", "futex", "--placements", "same", "--repeat", "1",
                "--iterations", "1", "--warmup", "0", "--cpus", "1",
                "--vcpu-cpus", str(allowed[0]), "--io-cpus", str(allowed[1]),
            ])

            def fake_run(config, *, input_stream=None, console_stream=None):
                del input_stream, console_stream
                log = (
                    "SCHED_BASELINE_RUN schema=thekernel-scheduler-baseline-run-v1 "
                    "arch=x86_64 workload=futex placement=same iterations=1 "
                    "warmup=0 cpus=1 cpu_work=1\n"
                    "SCHED_BASELINE_SAMPLE schema=thekernel-scheduler-baseline-sample-v1 "
                    "workload=futex placement=same worker=0 sample=0 latency_ns=1\n"
                )
                if complete_guest:
                    log += (
                        "SCHED_BASELINE_RESULT schema=thekernel-scheduler-baseline-result-v1 "
                        "workload=futex placement=same status=ok count=1 p50_ns=1 "
                        "p99_ns=1 p999_ns=1 checksum=7122294161688811748\n"
                        "SCHED_BASELINE_DONE schema=thekernel-scheduler-baseline-run-v1 "
                        "workload=futex placement=same\n"
                        "SCHED_BASELINE_EXIT status=0\n"
                    )
                config.log_path.write_text(log, encoding="utf-8")
                vcpu = tuple(int(value) for value in os.environ["THEKERNEL_KVM_VCPU_CPUS"].split(","))
                io = tuple(int(value) for value in os.environ["THEKERNEL_KVM_IO_CPUS"].split(","))
                self.write_pin_report(
                    config.workdir / "thread-pinning.json", vcpu_cpus=vcpu, io_cpus=io,
                    count=int(os.environ["THEKERNEL_KVM_VCPU_COUNT"]),
                )
                if not complete_pin:
                    pin_path = config.workdir / "thread-pinning.json"
                    pin_payload = json.loads(pin_path.read_text(encoding="utf-8"))
                    pin_payload["exit_readback_proof"] = False
                    pin_path.write_text(json.dumps(pin_payload), encoding="utf-8")
                return type("Result", (), {"returncode": returncode})()

            with patch("tools.kvm_scheduler_baseline.run", side_effect=fake_run):
                from tools.kvm_scheduler_baseline import run_command

                self.assertEqual(run_command(args), 1)
            manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
            raw = (output / "raw-samples.tsv").read_text(encoding="utf-8")
            summary = json.loads((output / "summary.json").read_text(encoding="utf-8"))
            return manifest, raw, summary

    def test_incomplete_guest_samples_are_not_aggregated(self) -> None:
        manifest, raw, summary = self.run_mocked_baseline(returncode=0, complete_guest=False)
        self.assertEqual(manifest["runs"][0]["status"], "incomplete")
        self.assertEqual(len(raw.splitlines()), 1)
        self.assertEqual(summary["raw_sample_count"], 0)
        self.assertEqual(summary["runs"], [])

    def test_nonzero_runner_samples_are_not_aggregated(self) -> None:
        manifest, raw, summary = self.run_mocked_baseline(returncode=4, complete_guest=True)
        self.assertEqual(manifest["runs"][0]["status"], "runner-error")
        self.assertEqual(len(raw.splitlines()), 1)
        self.assertEqual(summary["raw_sample_count"], 0)
        self.assertEqual(summary["runs"], [])

    def test_zero_runner_with_missing_pin_proof_is_explicitly_unsupported(self) -> None:
        manifest, raw, summary = self.run_mocked_baseline(
            returncode=0, complete_guest=True, complete_pin=False
        )
        self.assertEqual(manifest["runs"][0]["status"], "unsupported")
        self.assertEqual(len(raw.splitlines()), 1)
        self.assertEqual(summary["raw_sample_count"], 0)

    def test_guest_helper_emits_raw_samples_for_each_workload(self) -> None:
        for workload in ("futex", "pipe", "cpu-worker"):
            with self.subTest(workload=workload):
                result = self.run_helper(workload, "same")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("SCHED_BASELINE_RUN schema=thekernel-scheduler-baseline-run-v1", result.stdout)
                self.assertIn("SCHED_BASELINE_DONE schema=thekernel-scheduler-baseline-run-v1", result.stdout)
                samples = [line for line in result.stdout.splitlines() if line.startswith("SCHED_BASELINE_SAMPLE ")]
                self.assertEqual(len(samples), 16 if workload == "cpu-worker" else 8)
                self.assertTrue(all("latency_ns=" in line for line in samples))
                self.assertRegex(result.stdout, r"SCHED_BASELINE_RESULT .* status=ok count=(8|16) .* p999_ns=[1-9][0-9]*")

    def test_c_checksum_self_oracle_matches_parser_encoding(self) -> None:
        result = subprocess.run(
            [str(self.binary), "--selftest-checksum"],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(
            result.stdout,
            r"SCHED_BASELINE_CHECKSUM_SELFTEST status=ok checksum=5931715932612696898",
        )
        self.assertEqual(
            scheduler_sample_checksum(
                (
                    Sample("linux", 1, "futex", "same", 0, 0, 1),
                    Sample(
                        "linux", 1, "futex", "same", 1, 7,
                        0x0123456789ABCDEF,
                    ),
                )
            ),
            5931715932612696898,
        )

    def test_guest_checksum_round_trips_parser(self) -> None:
        for workload in ("futex", "pipe", "cpu-worker"):
            with self.subTest(workload=workload):
                result = self.run_helper(workload, "same")
                self.assertEqual(result.returncode, 0, result.stderr)
                with tempfile.TemporaryDirectory() as directory:
                    path = Path(directory) / "console.log"
                    path.write_text(
                        result.stdout + "SCHED_BASELINE_EXIT status=0\n",
                        encoding="utf-8",
                    )
                    guest = parse_guest_log(path, target="linux", repeat=1)
                self.assertEqual(guest.status, "ok")
                self.assertIsNone(guest.reason)

    def test_cross_cpu_futex_and_pipe_are_supported(self) -> None:
        for workload in ("futex", "pipe"):
            with self.subTest(workload=workload):
                result = self.run_helper(workload, "cross")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("placement=cross", result.stdout)
                self.assertIn("status=ok count=8", result.stdout)

    def test_quantiles_are_nearest_rank_and_raw_stats_round_trip(self) -> None:
        self.assertEqual(nearest_rank([9, 1, 4, 2], 500), 2)
        self.assertEqual(stats([9, 1, 4, 2]), {"count": 4, "p50_ns": 2, "p99_ns": 9, "p999_ns": 9})
        self.assertEqual(
            scheduler_sample_checksum(
                (Sample("linux", 1, "futex", "same", 0, 0, 1),)
            ),
            7122294161688811748,
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "raw.tsv"
            path.write_text("\t".join(RAW_COLUMNS) + "\n", encoding="utf-8")
            self.assertEqual(_read_raw(path), ())

    def test_checksum_mismatch_rejects_spliced_samples(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "console.log"
            path.write_text(
                "SCHED_BASELINE_RUN schema=thekernel-scheduler-baseline-run-v1 "
                "arch=x86_64 workload=futex placement=same iterations=1 warmup=0 "
                "cpus=1 cpu_work=1\n"
                "SCHED_BASELINE_SAMPLE schema=thekernel-scheduler-baseline-sample-v1 "
                "workload=futex placement=same worker=0 sample=0 latency_ns=1\n"
                "SCHED_BASELINE_RESULT schema=thekernel-scheduler-baseline-result-v1 "
                "workload=futex placement=same status=ok count=1 p50_ns=1 p99_ns=1 p999_ns=1 checksum=0\n"
                "SCHED_BASELINE_DONE schema=thekernel-scheduler-baseline-run-v1 workload=futex placement=same\n"
                "SCHED_BASELINE_EXIT status=0\n",
                encoding="utf-8",
            )
            guest = parse_guest_log(path, target="linux", repeat=1)
        self.assertEqual(guest.status, "incomplete")
        self.assertEqual(guest.reason, "result_checksum_mismatch")

    def test_kvm_command_has_explicit_accel_cpu_and_dedicated_iothread(self) -> None:
        command = build_qemu_command(
            arch="x86_64",
            kernel=Path("kernel"),
            rootfs=Drive(Path("root.img"), "snapshot"),
            esp=Drive(Path("esp.img"), "snapshot"),
            ovmf_code=Path("OVMF_CODE.fd"),
            ovmf_vars=Path("OVMF_VARS.fd"),
            cpus=2,
            accel="kvm",
            cpu="host",
            iothread_id="baseline-io",
            extra_args=("-name", "guest=scheduler-baseline,debug-threads=on"),
        )
        text = " ".join(command)
        self.assertIn("-machine q35", text)
        self.assertIn("-accel kvm", text)
        self.assertIn("-cpu host", text)
        self.assertIn("iothread,id=baseline-io", text)
        self.assertIn("virtio-blk-pci,drive=rootfs,iothread=baseline-io", text)
        self.assertIn("guest=scheduler-baseline,debug-threads=on", text)

    def test_scheduler_guest_command_drains_serial_before_poweroff(self) -> None:
        from tools.kvm_scheduler_baseline import _build_commands

        args = build_parser().parse_args([
            "run", "--target", "linux", "--linux-kernel", "bzImage",
            "--linux-rootfs", "rootfs.img", "--output", "out",
        ])
        command = _build_commands(args, "futex", "same").decode()
        self.assertIn("/proc/sys/kernel/printk", command)
        self.assertIn("/bin/busybox sleep 1", command)
        self.assertLess(command.index("SCHED_BASELINE_EXIT"), command.index("poweroff"))

    def test_linux_lane_requires_only_kernel_and_rootfs_and_uses_direct_boot(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "bzImage"
            rootfs = root / "rootfs.img"
            initrd = root / "initrd.img"
            kernel.write_bytes(b"linux")
            rootfs.write_bytes(b"ext4")
            initrd.write_bytes(b"initrd")
            args = build_parser().parse_args([
                "run", "--target", "linux",
                "--linux-kernel", str(kernel),
                "--linux-rootfs", str(rootfs),
                "--linux-initrd", str(initrd),
                "--output", str(root / "output"),
            ])
            images = _target_images(args, "linux")
            self.assertTrue(images.direct_kernel)
            self.assertIsNone(images.esp)
            self.assertEqual(images.initrd, initrd.resolve())
            self.assertEqual(images.kernel, kernel.resolve())
            self.assertEqual(images.rootfs, rootfs.resolve())
            self.assertEqual(images.cmdline, DEFAULT_LINUX_CMDLINE)
            command = build_qemu_command(
                arch="x86_64",
                kernel=images.kernel,
                rootfs=Drive(images.rootfs, "snapshot"),
                direct_kernel=images.direct_kernel,
                extra_args=(
                    "-initrd", str(images.initrd),
                    "-append", images.cmdline or "",
                ),
            )
            self.assertIn("-kernel", command)
            self.assertIn("-initrd", command)
            self.assertIn(str(initrd.resolve()), command)
            self.assertIn("-append", command)
            self.assertIn(DEFAULT_LINUX_CMDLINE, command)
            self.assertNotIn("if=pflash", " ".join(command))

    def test_linux_run_config_and_manifest_record_direct_boot(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "bzImage"
            rootfs = root / "rootfs.img"
            initrd = root / "initrd.img"
            kernel.write_bytes(b"linux")
            rootfs.write_bytes(b"ext4")
            initrd.write_bytes(b"initrd")
            output = root / "output"
            allowed = sorted(os.sched_getaffinity(0))
            if len(allowed) < 2:
                self.skipTest("host exposes fewer than two CPUs")
            args = build_parser().parse_args([
                "run", "--target", "linux",
                "--linux-kernel", str(kernel),
                "--linux-rootfs", str(rootfs),
                "--linux-initrd", str(initrd),
                "--output", str(output), "--workloads", "futex",
                "--placements", "same", "--repeat", "1", "--iterations", "1",
                "--warmup", "0", "--cpus", "1",
                "--vcpu-cpus", str(allowed[0]),
                "--io-cpus", str(allowed[1]),
            ])
            configs = []

            def fake_run(config, *, input_stream=None, console_stream=None):
                del input_stream, console_stream
                configs.append(config)
                config.log_path.write_text(
                    "SCHED_BASELINE_RUN schema=thekernel-scheduler-baseline-run-v1 "
                    "arch=x86_64 workload=futex placement=same iterations=1 "
                    "warmup=0 cpus=1 cpu_work=1\n"
                    "SCHED_BASELINE_SAMPLE schema=thekernel-scheduler-baseline-sample-v1 "
                    "workload=futex placement=same worker=0 sample=0 latency_ns=1\n"
                    "SCHED_BASELINE_RESULT schema=thekernel-scheduler-baseline-result-v1 "
                    "workload=futex placement=same status=ok count=1 p50_ns=1 "
                    "p99_ns=1 p999_ns=1 checksum=7122294161688811748\n"
                    "SCHED_BASELINE_DONE schema=thekernel-scheduler-baseline-run-v1 "
                    "workload=futex placement=same\n"
                    "SCHED_BASELINE_EXIT status=0\n",
                    encoding="utf-8",
                )
                vcpu = tuple(int(value) for value in os.environ["THEKERNEL_KVM_VCPU_CPUS"].split(","))
                io = tuple(int(value) for value in os.environ["THEKERNEL_KVM_IO_CPUS"].split(","))
                self.write_pin_report(
                    config.workdir / "thread-pinning.json",
                    vcpu_cpus=vcpu, io_cpus=io,
                    count=int(os.environ["THEKERNEL_KVM_VCPU_COUNT"]),
                )
                return type("Result", (), {"returncode": 0})()

            with patch("tools.kvm_scheduler_baseline.run", side_effect=fake_run):
                from tools.kvm_scheduler_baseline import run_command

                self.assertEqual(run_command(args), 0)
            self.assertEqual(len(configs), 1)
            self.assertTrue(configs[0].direct_kernel)
            self.assertIsNone(configs[0].esp)
            self.assertEqual(configs[0].extra_args[0:2], ("-initrd", str(initrd.resolve())))
            self.assertIn("-append", configs[0].extra_args)
            self.assertIn(DEFAULT_LINUX_CMDLINE, configs[0].extra_args)
            manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["targets"]["linux"]["boot"], "direct-kernel")
            self.assertIsNone(manifest["targets"]["linux"]["firmware"])
            self.assertIsNone(manifest["targets"]["linux"]["esp"])
            self.assertEqual(manifest["targets"]["linux"]["initrd"], str(initrd.resolve()))

    def test_linux_esp_option_is_not_part_of_the_cli_contract(self) -> None:
        with self.assertRaises(SystemExit):
            build_parser().parse_args([
                "run", "--target", "linux", "--linux-esp", "esp.img",
                "--output", "out",
            ])

    def test_baseline_cli_help_works_as_script_and_module(self) -> None:
        for script, module, label in (
            (
                REPO_ROOT / "tools" / "kvm_subsystem_baseline.py",
                "tools.kvm_subsystem_baseline",
                "subsystem",
            ),
            (
                REPO_ROOT / "tools" / "kvm_scheduler_baseline.py",
                "tools.kvm_scheduler_baseline",
                "scheduler",
            ),
        ):
            with self.subTest(label=label, form="script"):
                result = subprocess.run(
                    [sys.executable, str(script), "--help"],
                    cwd=REPO_ROOT,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("usage:", result.stdout)
            with self.subTest(label=label, form="module"):
                result = subprocess.run(
                    [sys.executable, "-m", module, "--help"],
                    cwd=REPO_ROOT,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("usage:", result.stdout)

    def test_thekernel_lane_never_consumes_linux_initrd(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kernel = root / "thekernel.elf"
            rootfs = root / "rootfs.img"
            esp = root / "shell.esp"
            linux_kernel = root / "bzImage"
            linux_rootfs = root / "linux-rootfs.img"
            initrd = root / "initrd.img"
            for artifact in (kernel, rootfs, esp, linux_kernel, linux_rootfs, initrd):
                artifact.write_bytes(b"artifact")
            args = build_parser().parse_args([
                "run", "--target", "both",
                "--kernel", str(kernel), "--rootfs", str(rootfs), "--esp", str(esp),
                "--linux-kernel", str(linux_kernel),
                "--linux-rootfs", str(linux_rootfs),
                "--linux-initrd", str(initrd),
                "--output", str(root / "output"),
            ])
            images = _target_images(args, "thekernel")
            self.assertFalse(images.direct_kernel)
            self.assertIsNone(images.initrd)


if __name__ == "__main__":
    unittest.main()
