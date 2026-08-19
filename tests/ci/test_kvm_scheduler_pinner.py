from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tools.kvm_scheduler_pinner import (
    BackendIdentityUnavailable,
    CpuTopologyUnavailable,
    KvmNxPrearm,
    PIN_REPORT_SCHEMA,
    _housekeeping_cpus,
    _pre_exit_affinity_safe,
    _read_smt_topology,
    _smt_siblings,
    classify_thread,
    exit_role_closed,
    is_untraced_kvm_nx_worker,
    parse_external_backend_identities,
    task_name,
    untraced_worker_left_qemu_group,
    write_report,
)


class SchedulerPinnerTests(unittest.TestCase):
    def test_kvm_nx_prearm_holds_then_releases_vcpu(self) -> None:
        state = KvmNxPrearm(enabled=True, deadline=10.0)
        self.assertEqual(state.vcpu_cpus((2, 3), (6, 7), 1), (6, 7))
        self.assertFalse(state.timed_out(9.0))
        self.assertIsNone(
            state.observe_worker(
                41, (6, 7), housekeeping=(6, 7), measurement={2, 3, 4}
            )
        )
        self.assertTrue(state.armed)
        self.assertEqual(state.vcpu_cpus((2, 3), (6, 7), 1), (3,))
        self.assertIsNone(
            state.observe_worker(
                41, (6,), housekeeping=(6, 7), measurement={2, 3, 4}
            )
        )
        self.assertEqual(
            state.observe_worker(
                42, (6,), housekeeping=(6, 7), measurement={2, 3, 4}
            ),
            "new-worker-after-arm",
        )

    def test_kvm_nx_prearm_rejects_overlap_and_timeout(self) -> None:
        state = KvmNxPrearm(enabled=True, deadline=10.0)
        self.assertEqual(
            state.observe_worker(
                41, (3,), housekeeping=(6, 7), measurement={2, 3, 4}
            ),
            "worker-not-on-housekeeping",
        )
        self.assertFalse(state.armed)
        self.assertTrue(state.timed_out(10.0))

    def test_exit_role_closes_even_when_an_earlier_task_failed_proof(self) -> None:
        self.assertTrue(exit_role_closed(False, True))
        self.assertFalse(exit_role_closed(True, False))

    def test_kvm_nx_worker_requires_exact_comm_tgid_and_user_worker_flag(self) -> None:
        with (
            patch("tools.kvm_scheduler_pinner.process_tgid", return_value=41),
            patch(
                "tools.kvm_scheduler_pinner.task_kernel_flags",
                return_value=0x00004000,
            ),
        ):
            self.assertTrue(is_untraced_kvm_nx_worker(42, 41, "kvm-nx-lpage-re"))
            self.assertFalse(is_untraced_kvm_nx_worker(42, 40, "kvm-nx-lpage-re"))
            self.assertFalse(is_untraced_kvm_nx_worker(42, 41, "worker"))
        with (
            patch("tools.kvm_scheduler_pinner.process_tgid", return_value=41),
            patch("tools.kvm_scheduler_pinner.task_kernel_flags", return_value=0),
        ):
            self.assertFalse(is_untraced_kvm_nx_worker(42, 41, "kvm-nx-lpage-re"))

    def test_untraced_worker_closes_only_after_leaving_qemu_group(self) -> None:
        with patch("tools.kvm_scheduler_pinner.process_tgid", return_value=41):
            self.assertFalse(untraced_worker_left_qemu_group(42, 41))
        with patch("tools.kvm_scheduler_pinner.process_tgid", return_value=None):
            self.assertTrue(untraced_worker_left_qemu_group(42, 41))
        with patch("tools.kvm_scheduler_pinner.process_tgid", return_value=99):
            self.assertTrue(untraced_worker_left_qemu_group(42, 41))

    def test_task_name_reads_exact_tid_without_group_enumeration(self) -> None:
        with patch.object(
            Path, "read_text", autospec=True, return_value="daemon-leader\n"
        ) as read:
            self.assertEqual(task_name(41), "daemon-leader")
        read.assert_called_once_with(Path("/proc/41/comm"), encoding="ascii")

    def test_missing_smt_sibling_file_is_explicitly_unsupported(self) -> None:
        with patch.object(Path, "read_text", side_effect=FileNotFoundError("missing")):
            with self.assertRaises(CpuTopologyUnavailable):
                _smt_siblings(0)

    def test_smt_sibling_sets_must_be_reciprocal(self) -> None:
        def asymmetric(cpu: int) -> frozenset[int]:
            return frozenset({0, 1}) if cpu == 0 else frozenset({1})

        with patch("tools.kvm_scheduler_pinner._smt_siblings", side_effect=asymmetric):
            with self.assertRaisesRegex(CpuTopologyUnavailable, "reciprocal"):
                _read_smt_topology({0, 1})

    def test_housekeeping_excludes_measurement_smt_siblings(self) -> None:
        self.assertEqual(
            _housekeeping_cpus(
                (), {2}, {0, 1, 2, 3}, measurement_smt_siblings=(1, 2)
            ),
            (0, 3),
        )
        with self.assertRaisesRegex(ValueError, "SMT siblings"):
            _housekeeping_cpus(
                (1,), {2}, {0, 1, 2, 3}, measurement_smt_siblings=(1, 2)
            )

    def test_exit_affinity_check_rejects_pre_pin_unknown_overlap(self) -> None:
        post_pin_housekeeping = (0,)
        self.assertFalse(_pre_exit_affinity_safe("unknown", (2,), {2, 3}))
        self.assertTrue(_pre_exit_affinity_safe("unknown", post_pin_housekeeping, {2, 3}))
        self.assertTrue(_pre_exit_affinity_safe("vcpu", (2,), {2, 3}))

    def test_exit_affinity_backend_policy_follows_requested_assignment(self) -> None:
        measurement = {2, 3}
        self.assertFalse(_pre_exit_affinity_safe("backend", (2,), measurement))
        self.assertTrue(
            _pre_exit_affinity_safe(
                "backend", (2,), measurement, requested_backend=(2,)
            )
        )

    def test_traced_fork_child_is_pinned_and_reported(self) -> None:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 4:
            self.skipTest("requires four host CPUs for the placement proof")
        vcpu, io_cpu, _, housekeeping = available[:4]
        code = (
            "import os,threading,time; "
            "worker=threading.Thread(target=lambda: time.sleep(0.03), name='IO thread'); "
            "worker.start(); child=os.fork(); "
            "(time.sleep(0.04) if child == 0 else os.waitpid(child, 0)); "
            "worker.join()"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "pin.json"
            environment = os.environ.copy()
            environment.update(
                {
                    "THEKERNEL_KVM_QEMU": sys.executable,
                    "THEKERNEL_KVM_VCPU_CPUS": str(vcpu),
                    "THEKERNEL_KVM_IO_CPUS": str(io_cpu),
                    "THEKERNEL_KVM_BACKEND_CPUS": "",
                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS": str(housekeeping),
                    "THEKERNEL_KVM_VCPU_COUNT": "1",
                    "THEKERNEL_KVM_PIN_REPORT": str(report),
                }
            )
            result = subprocess.run(
                [sys.executable, "tools/kvm_scheduler_pinner.py", "-c", code],
                cwd=Path(__file__).parents[2],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5.0,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(report.read_text(encoding="utf-8"))
        self.assertGreaterEqual(payload["clone_event_count"], 2)
        self.assertTrue(payload["ptrace_clone_events"])
        self.assertTrue(
            set(payload["measurement_cpus"]).issubset(
                set(payload["measurement_smt_siblings"])
            )
        )
        self.assertFalse(
            set(payload["housekeeping_cpus"])
            & set(payload["measurement_smt_siblings"])
        )
        self.assertEqual(payload["unknown_status"], "ok")
        self.assertTrue(payload["external_processes"])
        self.assertTrue(payload["exit_readback_proof"])
        self.assertTrue(payload["exit_readback_tids"])
        self.assertTrue(
            any(
                record["main_tid"] != payload["pid"]
                for record in payload["external_processes"]
            )
        )

    def test_traced_clone_process_is_tracked_as_external(self) -> None:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 4:
            self.skipTest("requires four host CPUs for the placement proof")
        vcpu, io_cpu, _, housekeeping = available[:4]
        code = """
import ctypes
import os
import time

libc = ctypes.CDLL(None, use_errno=True)
libc.clone.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p]
libc.clone.restype = ctypes.c_int
libc.usleep.argtypes = [ctypes.c_uint]
Callback = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p)

def child(_):
    libc.usleep(20000)
    return 0

callback = Callback(child)
stack = ctypes.create_string_buffer(65536)
# CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND with SIGCHLD: a separate
# process created by clone(2), which must follow the CLONE event path.
flags = 0x00000100 | 0x00000200 | 0x00000400 | 0x00000800 | 17
child_pid = libc.clone(callback, ctypes.addressof(stack) + len(stack), flags, None)
if child_pid <= 0:
    raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
os.waitpid(child_pid, 0)
time.sleep(0.05)
"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "pin.json"
            environment = os.environ.copy()
            environment.update(
                {
                    "THEKERNEL_KVM_QEMU": sys.executable,
                    "THEKERNEL_KVM_VCPU_CPUS": str(vcpu),
                    "THEKERNEL_KVM_IO_CPUS": str(io_cpu),
                    "THEKERNEL_KVM_BACKEND_CPUS": "",
                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS": str(housekeeping),
                    "THEKERNEL_KVM_VCPU_COUNT": "1",
                    "THEKERNEL_KVM_PIN_REPORT": str(report),
                }
            )
            result = subprocess.run(
                [sys.executable, "tools/kvm_scheduler_pinner.py", "-c", code],
                cwd=Path(__file__).parents[2],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5.0,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(report.read_text(encoding="utf-8"))
        self.assertGreaterEqual(payload["clone_event_count"], 1)
        self.assertTrue(payload["ptrace_clone_events"])
        self.assertTrue(
            any(record["main_tid"] != payload["pid"] for record in payload["external_processes"])
        )

    def test_external_cpu_named_helper_stays_unknown_while_live(self) -> None:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 4:
            self.skipTest("requires four host CPUs for the placement proof")
        vcpu, io_cpu, _, housekeeping = available[:4]
        code = """
import ctypes
import os
import time

libc = ctypes.CDLL(None, use_errno=True)
libc.prctl.argtypes = [ctypes.c_int, ctypes.c_char_p,
                       ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
child = os.fork()
if child == 0:
    if libc.prctl(15, b"CPU 0/KVM", 0, 0, 0) != 0:
        os._exit(11)
    time.sleep(0.08)
else:
    os.waitpid(child, 0)
    time.sleep(0.03)
"""
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "pin.json"
            environment = os.environ.copy()
            environment.update(
                {
                    "THEKERNEL_KVM_QEMU": sys.executable,
                    "THEKERNEL_KVM_VCPU_CPUS": str(vcpu),
                    "THEKERNEL_KVM_IO_CPUS": str(io_cpu),
                    "THEKERNEL_KVM_BACKEND_CPUS": "",
                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS": str(housekeeping),
                    "THEKERNEL_KVM_VCPU_COUNT": "1",
                    "THEKERNEL_KVM_PIN_REPORT": str(report),
                }
            )
            result = subprocess.run(
                [sys.executable, "tools/kvm_scheduler_pinner.py", "-c", code],
                cwd=Path(__file__).parents[2],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5.0,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(report.read_text(encoding="utf-8"))
        external = payload["external_processes"]
        self.assertTrue(any(record["name"] == "CPU 0/KVM" for record in external))
        spoof_pids = {record["pid"] for record in external}
        self.assertFalse(
            spoof_pids & {
                record["tid"] for record in payload["vcpu_threads"].values()
            }
        )
        self.assertFalse(spoof_pids & {record["tid"] for record in payload["io_threads"]})

    def test_external_io_named_helper_stays_unknown_at_exit(self) -> None:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 4:
            self.skipTest("requires four host CPUs for the placement proof")
        vcpu, io_cpu, _, housekeeping = available[:4]
        code = """
import ctypes
import os
import time

libc = ctypes.CDLL(None, use_errno=True)
libc.prctl.argtypes = [ctypes.c_int, ctypes.c_char_p,
                       ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
child = os.fork()
if child == 0:
    if libc.prctl(15, b"IO thread", 0, 0, 0) != 0:
        os._exit(11)
    os._exit(0)
else:
    os.waitpid(child, 0)
    time.sleep(0.03)
"""
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "pin.json"
            environment = os.environ.copy()
            environment.update(
                {
                    "THEKERNEL_KVM_QEMU": sys.executable,
                    "THEKERNEL_KVM_VCPU_CPUS": str(vcpu),
                    "THEKERNEL_KVM_IO_CPUS": str(io_cpu),
                    "THEKERNEL_KVM_BACKEND_CPUS": "",
                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS": str(housekeeping),
                    "THEKERNEL_KVM_VCPU_COUNT": "1",
                    "THEKERNEL_KVM_PIN_REPORT": str(report),
                }
            )
            result = subprocess.run(
                [sys.executable, "tools/kvm_scheduler_pinner.py", "-c", code],
                cwd=Path(__file__).parents[2],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5.0,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(report.read_text(encoding="utf-8"))
        external = payload["external_processes"]
        self.assertTrue(any(record["name"] == "IO thread" for record in external))
        spoof_pids = {record["pid"] for record in external}
        self.assertFalse(
            spoof_pids & {
                record["tid"] for record in payload["vcpu_threads"].values()
            }
        )
        self.assertFalse(spoof_pids & {record["tid"] for record in payload["io_threads"]})
        self.assertTrue(payload["exit_readback_proof"])

    def test_external_backend_identity_contract_is_exact(self) -> None:
        identity = {
            "pid": 41,
            "tgid": 41,
            "exe": "/usr/bin/passt",
            "starttime": 123,
        }
        self.assertEqual(
            parse_external_backend_identities(json.dumps([identity])),
            (identity,),
        )
        for malformed in (
            json.dumps({"pid": 41}),
            json.dumps([{**identity, "comm": "vhost"}]),
            json.dumps([{**identity, "pid": 0}]),
            json.dumps([{**identity}, {**identity}]),
            json.dumps([{**identity, "exe": "/tmp/\u0000evil"}]),
        ):
            with self.subTest(malformed=malformed):
                with self.assertRaises(BackendIdentityUnavailable):
                    parse_external_backend_identities(malformed)

    def test_external_vhost_spoof_stays_housekeeping_while_live(self) -> None:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 4:
            self.skipTest("requires four host CPUs for the placement proof")
        vcpu, io_cpu, backend_cpu, housekeeping = available[:4]
        code = """
import ctypes
import os
import time

libc = ctypes.CDLL(None, use_errno=True)
libc.prctl.argtypes = [ctypes.c_int, ctypes.c_char_p,
                       ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
child = os.fork()
if child == 0:
    if libc.prctl(15, b"vhost-evil", 0, 0, 0) != 0:
        os._exit(11)
    time.sleep(0.08)
else:
    os.waitpid(child, 0)
    time.sleep(0.03)
"""
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "pin.json"
            environment = os.environ.copy()
            environment.update(
                {
                    "THEKERNEL_KVM_QEMU": sys.executable,
                    "THEKERNEL_KVM_VCPU_CPUS": str(vcpu),
                    "THEKERNEL_KVM_IO_CPUS": str(io_cpu),
                    "THEKERNEL_KVM_BACKEND_CPUS": str(backend_cpu),
                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS": str(housekeeping),
                    "THEKERNEL_KVM_VCPU_COUNT": "1",
                    "THEKERNEL_KVM_PIN_REPORT": str(report),
                }
            )
            result = subprocess.run(
                [sys.executable, "tools/kvm_scheduler_pinner.py", "-c", code],
                cwd=Path(__file__).parents[2],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5.0,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(report.read_text(encoding="utf-8"))
        spoof = next(record for record in payload["external_processes"] if record["name"] == "vhost-evil")
        self.assertFalse(spoof["backend_authorized"])
        self.assertEqual(spoof["affinity"], [housekeeping])
        self.assertNotIn(spoof["pid"], {record["tid"] for record in payload["backend_threads"]})

    def test_external_backend_spoof_stays_housekeeping_at_exit(self) -> None:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 4:
            self.skipTest("requires four host CPUs for the placement proof")
        vcpu, io_cpu, backend_cpu, housekeeping = available[:4]
        code = """
import ctypes
import os
import time

libc = ctypes.CDLL(None, use_errno=True)
libc.prctl.argtypes = [ctypes.c_int, ctypes.c_char_p,
                       ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
child = os.fork()
if child == 0:
    if libc.prctl(15, b"backend-spoof", 0, 0, 0) != 0:
        os._exit(11)
    os._exit(0)
else:
    os.waitpid(child, 0)
    time.sleep(0.03)
"""
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "pin.json"
            environment = os.environ.copy()
            environment.update(
                {
                    "THEKERNEL_KVM_QEMU": sys.executable,
                    "THEKERNEL_KVM_VCPU_CPUS": str(vcpu),
                    "THEKERNEL_KVM_IO_CPUS": str(io_cpu),
                    "THEKERNEL_KVM_BACKEND_CPUS": str(backend_cpu),
                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS": str(housekeeping),
                    "THEKERNEL_KVM_VCPU_COUNT": "1",
                    "THEKERNEL_KVM_PIN_REPORT": str(report),
                }
            )
            result = subprocess.run(
                [sys.executable, "tools/kvm_scheduler_pinner.py", "-c", code],
                cwd=Path(__file__).parents[2],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5.0,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(report.read_text(encoding="utf-8"))
        spoof = next(record for record in payload["external_processes"] if record["name"] == "backend-spoof")
        self.assertFalse(spoof["backend_authorized"])
        self.assertEqual(spoof["affinity"], [housekeeping])
        self.assertNotIn(spoof["pid"], {record["tid"] for record in payload["backend_threads"]})
        self.assertTrue(payload["exit_readback_proof"])

    def test_traced_clone_thread_is_tracked_in_qemu_task_set(self) -> None:
        available = sorted(os.sched_getaffinity(0))
        if len(available) < 4:
            self.skipTest("requires four host CPUs for the placement proof")
        vcpu, io_cpu, _, housekeeping = available[:4]
        code = """
import ctypes
import os
import time

libc = ctypes.CDLL(None, use_errno=True)
libc.clone.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int, ctypes.c_void_p]
libc.clone.restype = ctypes.c_int
libc.usleep.argtypes = [ctypes.c_uint]
Callback = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p)

def child(_):
    libc.usleep(20000)
    return 0

callback = Callback(child)
stack = ctypes.create_string_buffer(65536)
# CLONE_THREAD keeps the child in the QEMU/Python tgid.  It must be recorded
# as a thread identity, not silently treated as an external process.
flags = (0x00000100 | 0x00000200 | 0x00000400 | 0x00000800 |
         0x00010000)
child_tid = libc.clone(callback, ctypes.addressof(stack) + len(stack), flags, None)
if child_tid <= 0:
    raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))
time.sleep(0.08)
"""
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "pin.json"
            environment = os.environ.copy()
            environment.update(
                {
                    "THEKERNEL_KVM_QEMU": sys.executable,
                    "THEKERNEL_KVM_VCPU_CPUS": str(vcpu),
                    "THEKERNEL_KVM_IO_CPUS": str(io_cpu),
                    "THEKERNEL_KVM_BACKEND_CPUS": "",
                    "THEKERNEL_KVM_HOUSEKEEPING_CPUS": str(housekeeping),
                    "THEKERNEL_KVM_VCPU_COUNT": "1",
                    "THEKERNEL_KVM_PIN_REPORT": str(report),
                }
            )
            result = subprocess.run(
                [sys.executable, "tools/kvm_scheduler_pinner.py", "-c", code],
                cwd=Path(__file__).parents[2],
                env=environment,
                capture_output=True,
                text=True,
                timeout=5.0,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(report.read_text(encoding="utf-8"))
        self.assertGreaterEqual(payload["clone_event_count"], 1)
        self.assertTrue(payload["ptrace_clone_events"])
        self.assertFalse(payload["external_processes"])

    def test_thread_classes_are_explicit(self) -> None:
        self.assertEqual(classify_thread(10, 10, "qemu-system-x86_64"), "qemu-main")
        self.assertEqual(classify_thread(11, 10, "CPU 0/KVM"), "vcpu")
        self.assertEqual(classify_thread(12, 10, "IO thread"), "iothread")
        self.assertEqual(classify_thread(13, 10, "vhost-42"), "backend")
        self.assertEqual(classify_thread(14, 10, "mystery"), "unknown")

    def test_report_proves_unknown_threads_are_off_measurement_cpus(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pin.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                requested_backend=(4,),
                housekeeping=(0, 1),
                backend_threads=[{"tid": 13, "name": "vhost-42", "affinity": [4]}],
                qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
                ptrace_clone_events=True,
                clone_event_count=1,
                proof_failures=[
                    {
                        "reason": "untraced-kvm-worker-affinity-overlap",
                        "tid": 15,
                        "affinity": [2],
                        "measurement": [2, 3, 4],
                    }
                ],
            )
            payload = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(payload["schema"], PIN_REPORT_SCHEMA)
        self.assertTrue(payload["unknown_off_measurement"])
        self.assertEqual(payload["unknown_status"], "ok")
        self.assertEqual(payload["housekeeping_status"], "ok")
        self.assertEqual(payload["backend_status"], "ok")
        self.assertEqual(payload["launcher_affinity"], [0, 1])
        self.assertTrue(payload["process_inherited_housekeeping"])
        self.assertTrue(payload["new_threads_inherit_housekeeping"])
        self.assertEqual(
            payload["proof_failures"][0]["reason"],
            "untraced-kvm-worker-affinity-overlap",
        )

    def test_report_flags_unknown_measurement_overlap(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pin.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [2]}],
            )
            payload = json.loads(path.read_text(encoding="utf-8"))
        self.assertFalse(payload["unknown_off_measurement"])
        self.assertEqual(payload["unknown_status"], "measurement_overlap")

    def test_report_without_clone_proof_is_unsupported(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "pin.json"
            write_report(
                path,
                pid=10,
                expected_vcpu_count=1,
                vcpus={"0": {"tid": 11, "name": "CPU 0/KVM", "affinity": [2]}},
                io_threads=[{"tid": 12, "name": "IO thread", "affinity": [3]}],
                requested_vcpu=(2,),
                requested_io=(3,),
                housekeeping=(0, 1),
                qemu_main={"tid": 10, "name": "qemu", "affinity": [0, 1]},
                unknown_threads=[{"tid": 14, "name": "mystery", "affinity": [1]}],
            )
            payload = json.loads(path.read_text(encoding="utf-8"))
        self.assertFalse(payload["ptrace_clone_events"])
        self.assertEqual(payload["unknown_thread_proof"], "unsupported")
        self.assertEqual(payload["unknown_status"], "unsupported")


if __name__ == "__main__":
    unittest.main()
