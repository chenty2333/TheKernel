"""Equivalent KVM workload trials using the existing product QEMU runner.

Builds and artifact configuration validation belong to tools/thekernel.py.
This module runs already prepared drive-rootfs ESPs, validates observations,
and compares paired trials. It never selects a default kernel policy.
"""
from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass, replace
import itertools
import json
import math
import os
from pathlib import Path
import random
import re
import shutil
import sys
import tempfile
from typing import Literal

from .boot_artifacts import validate_linux_esp_kernel, validate_thekernel_esp_kernel, validate_linux_boot
from .model import Interaction, RunLimits, QmpControls
from .runner import RunConfig, RunnerError, run

COMPLETE_MARKER = "THEKERNEL_BENCH_EXIT_ZERO"
SHELL_MARKER = "THEKERNEL_SHELL_READY"
PRESSURES = ("none", "cpu", "io", "mixed")
IO_FIELDS = ("workload", "block_bytes", "queue_depth", "resources", "cache", "operation", "durability")


@dataclass(frozen=True)
class BenchmarkTarget:
    name: Literal["baseline", "linux", "candidate"]
    kernel: Path
    esp: Path


@dataclass(frozen=True)
class BenchmarkConfig:
    targets: tuple[BenchmarkTarget, ...]
    rootfs: Path
    workdir: Path
    suite: Literal["scheduler", "io", "all"]
    iterations: int = 1000
    trials: int = 10
    cpus: int = 4
    memory: str = "4G"
    host_cpus: tuple[int, ...] = ()
    timeout: float = 1800.0
    qemu_binary: str | None = None


def _scenario(row: dict) -> tuple:
    if row.get("suite") == "scheduler":
        return ("scheduler", row.get("pressure"))
    return ("io", *(row.get(field) for field in IO_FIELDS))


def _expected_scenarios(suite: str) -> set[tuple]:
    keys = set()
    if suite in {"scheduler", "all"}:
        keys.update(("scheduler", pressure) for pressure in PRESSURES)
    if suite in {"io", "all"}:
        for (workload, size), depth, resources, cache, (operation, durability) in itertools.product(
            (("random", 4096), ("sequential", 131072)), (1, 8, 32),
            ("ordinary", "fixed"), ("buffered", "direct"),
            (("read", "none"), ("write", "none"), ("write", "fsync_per_batch")),
        ):
            keys.add(("io", workload, size, depth, resources, cache, operation, durability))
    return keys


def _number(row: dict, name: str, *, positive: bool = True) -> float:
    value = row.get(name)
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise RunnerError(f"benchmark field {name} is not a finite number")
    if (positive and value <= 0) or (not positive and value < 0):
        raise RunnerError(f"benchmark field {name} is outside its valid range")
    return float(value)


def parse_benchmark_log(path: Path, suite: str, iterations: int, *, linux: bool = False,
                        workers: int | None = None) -> dict[tuple, dict]:
    """Reject partial/duplicate matrices, silent failures and mismatched work."""
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    if lines.count(COMPLETE_MARKER) != 1:
        raise RunnerError(f"benchmark completion marker missing or duplicated: {path}")
    complete_index = lines.index(COMPLETE_MARKER)
    if any(line.lstrip().startswith('{"suite":') for line in lines[complete_index + 1:]):
        raise RunnerError(f"benchmark records follow completion: {path}")
    if re.search(r"^THEKERNEL_\S*(?:FAIL|SKIP)(?:\s|$)|^.*(?:Kernel panic|scheduler timeout)", text, re.MULTILINE):
        raise RunnerError(f"benchmark reported failure: {path}")
    if linux:
        validate_linux_boot(text, path)
    rows: dict[tuple, dict] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith('{"suite":'):
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise RunnerError(f"invalid benchmark JSON in {path}: {error}") from error
        if row.get("iterations") != iterations:
            raise RunnerError(f"benchmark iteration count differs in {path}")
        _number(row, "elapsed_ns")
        measurement = row.get("measurement")
        if not isinstance(measurement, dict) or measurement.get("scope") != "foreground_caller":
            raise RunnerError("benchmark measurement scope differs")
        for field in ("cpu_user_ns", "cpu_system_ns", "voluntary_switches", "involuntary_switches", "cpu_migrations", "maxrss_kib"):
            value = measurement.get(field)
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise RunnerError(f"invalid benchmark measurement {field}")
        if row.get("suite") == "scheduler":
            if row.get("workload") != "pipe_handoff":
                raise RunnerError("unexpected scheduler workload")
            wake = row.get("wake_trace")
            if not isinstance(wake, dict) or wake.get("scope") != "handoff_child" or wake.get("clock") != "monotonic":
                raise RunnerError("wake trace scope or clock differs")
            samples = wake.get("samples")
            if isinstance(samples, bool) or not isinstance(samples, int) or samples <= 0:
                raise RunnerError("wake trace contains no valid samples")
            wake_quantiles = [_number(wake, f"wake_to_run_p{q}_ns", positive=False) for q in (50, 95, 99)]
            if wake_quantiles != sorted(wake_quantiles):
                raise RunnerError("wake trace quantiles are not monotonic")
            quantiles = [_number(row, f"handoff_p{q}_ns") for q in (50, 95, 99)]
            if quantiles != sorted(quantiles):
                raise RunnerError("scheduler quantiles are not monotonic")
            pressure = row.get("pressure")
            if pressure not in PRESSURES or not isinstance(row.get("workers"), list):
                raise RunnerError("scheduler pressure or worker records are invalid")
            worker_count = row.get("workers_per_kind")
            if isinstance(worker_count, bool) or not isinstance(worker_count, int) or not 1 <= worker_count <= 64:
                raise RunnerError("invalid scheduler pressure worker count")
            if workers is not None and worker_count != workers:
                raise RunnerError("scheduler pressure worker count differs")
            if row.get("zero_progress_workers") != 0:
                raise RunnerError("scheduler pressure worker made no progress")
            expected_cpu = row.get("workers_per_kind") if pressure in {"cpu", "mixed"} else 0
            expected_io = row.get("workers_per_kind") if pressure in {"io", "mixed"} else 0
            if (row.get("background_cpu_workers"), row.get("background_io_workers")) != (expected_cpu, expected_io):
                raise RunnerError("scheduler pressure topology differs")
            kinds = [worker.get("kind") for worker in row["workers"]]
            if (kinds.count("lcg_65536"), kinds.count("write_fsync_read_4k")) != (expected_cpu, expected_io):
                raise RunnerError("scheduler worker records do not match its pressure topology")
            if len(kinds) != expected_cpu + expected_io:
                raise RunnerError("unexpected scheduler worker kind")
            for worker in row["workers"]:
                _number(worker, "units")
                _number(worker, "units_per_second")
                gap = _number(worker, "max_progress_gap_ns")
                if gap > _number(row, "pressure_elapsed_ns"):
                    raise RunnerError("worker progress gap exceeds measurement window")
        elif row.get("suite") == "io":
            if row.get("bytes") != iterations * row.get("block_bytes", 0):
                raise RunnerError("I/O byte count differs")
            if row.get("includes_buffer_work") is not True:
                raise RunnerError("I/O timing semantics differ")
        else:
            raise RunnerError("unexpected benchmark suite")
        key = _scenario(row)
        if key in rows:
            raise RunnerError(f"duplicate benchmark scenario {key}")
        rows[key] = row
    if rows.keys() != _expected_scenarios(suite):
        raise RunnerError(f"benchmark matrix incomplete or unexpected: {path}")
    return rows


def _metrics(row: dict) -> dict[str, tuple[float, bool]]:
    """Metric value and whether smaller is better; compare identical scenarios."""
    measurement = row["measurement"]
    metrics = {f"foreground_{field}": (float(measurement[field]), True) for field in
               ("cpu_user_ns", "cpu_system_ns", "voluntary_switches", "involuntary_switches", "cpu_migrations", "maxrss_kib")}
    if row["suite"] == "io":
        metrics["elapsed_ns_per_operation"] = (float(row["elapsed_ns"]) / row["iterations"], True)
        return metrics
    metrics.update({f"handoff_p{q}_ns": (float(row[f"handoff_p{q}_ns"]), True) for q in (50, 95, 99)})
    metrics.update({f"wake_to_run_p{q}_ns": (float(row["wake_trace"][f"wake_to_run_p{q}_ns"]), True) for q in (50, 95, 99)})
    for kind, label in (("lcg_65536", "cpu"), ("write_fsync_read_4k", "io")):
        rates = [float(worker["units_per_second"]) for worker in row["workers"] if worker["kind"] == kind]
        if rates:
            metrics[f"background_{label}_units_per_second"] = (sum(rates), False)
            metrics[f"slowest_{label}_worker_units_per_second"] = (min(rates), False)
            metrics[f"{label}_jain_fairness"] = (sum(rates) ** 2 / (len(rates) * sum(rate ** 2 for rate in rates)), False)
            metrics[f"{label}_max_progress_gap_ns"] = (max(float(worker["max_progress_gap_ns"]) for worker in row["workers"] if worker["kind"] == kind), True)
    return metrics


def paired_improvement(reference: list[float], target: list[float], *, smaller_better: bool) -> dict:
    """Geometric mean paired change with a 95% percentile bootstrap interval.

    Trials, not individual handoffs/I/Os, are the resampling unit. A fixed PRNG
    seed makes reformatting the same observations reproducible. This interval
    does not establish semantic correctness or authorize default selection.
    """
    if len(reference) < 10 or len(reference) != len(target):
        raise RunnerError("paired benchmark comparison needs at least ten matching trials")
    if any(not math.isfinite(value) or value <= 0 for value in reference + target):
        raise RunnerError("paired benchmark metrics must be finite and positive")
    logs = [math.log(t / r) for r, t in zip(reference, target)]
    rng = random.Random(0)
    estimates = sorted(math.exp(sum(rng.choices(logs, k=len(logs))) / len(logs)) for _ in range(5000))
    point = math.exp(sum(logs) / len(logs))
    low, high = estimates[124], estimates[4874]
    if smaller_better:
        improvement, ci = 100 * (1 - point), (100 * (1 - high), 100 * (1 - low))
    else:
        improvement, ci = 100 * (point - 1), (100 * (low - 1), 100 * (high - 1))
    return {
        "improvement_percent": improvement,
        "ci95_improvement_percent": list(ci),
        "meets_10_percent_threshold": ci[0] >= 10,
        "point_regression_exceeds_5_percent": improvement < -5,
    }


def _validate_config(config: BenchmarkConfig) -> tuple[int, ...]:
    names = [target.name for target in config.targets]
    if set(names) not in ({"baseline", "linux"}, {"baseline", "linux", "candidate"}) or len(names) != len(set(names)):
        raise RunnerError("benchmark targets must contain baseline and linux, with at most one candidate")
    if config.suite not in {"scheduler", "io", "all"} or not 32 <= config.iterations <= 1000000:
        raise RunnerError("invalid benchmark suite or iteration count")
    if config.trials < 1 or config.cpus not in (1, 4) or not math.isfinite(config.timeout) or config.timeout <= 0:
        raise RunnerError("benchmark needs >=1 trial, 1/4 vCPUs and a positive timeout")
    available = os.sched_getaffinity(0)
    selected = config.host_cpus or tuple(sorted(available)[:config.cpus])
    if (any(type(cpu) is not int or cpu < 0 for cpu in selected)
        or len(selected) != len(set(selected)) or len(selected) < config.cpus or not set(selected) <= available):
        raise RunnerError("benchmark host CPU mask must provide unique available CPUs for every vCPU")
    for path in (config.rootfs, *(p for target in config.targets for p in (target.kernel, target.esp))):
        if not path.is_file() or not path.stat().st_size:
            raise RunnerError(f"benchmark input is missing or empty: {path}")
    for target in config.targets:
        if target.name == "linux":
            validate_linux_esp_kernel(target.kernel, target.esp)
        else:
            validate_thekernel_esp_kernel(target.kernel, target.esp)
    workdir = config.workdir.expanduser().resolve()
    # Product callers also use validate_storage. Keep the internal API safe when
    # exercised directly by tests or another in-tree runner.
    matches = []
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        fields = line.split()
        mount = Path(fields[4].replace(r"\040", " "))
        if workdir == mount or mount in workdir.parents:
            matches.append((len(mount.parts), fields[fields.index("-") + 1]))
    if (matches and max(matches)[1] in {"tmpfs", "ramfs"}) or any(
        workdir == base or base in workdir.parents for base in (Path("/tmp"), Path("/dev/shm"))
    ):
        raise RunnerError("benchmark artifacts must be stored outside tmpfs")
    return tuple(selected)


@contextmanager
def _execution_environment(cpus: tuple[int, ...], temporary: Path):
    previous_affinity = os.sched_getaffinity(0)
    previous_tmp = os.environ.get("TMPDIR")
    os.sched_setaffinity(0, cpus)
    os.environ["TMPDIR"] = str(temporary)
    try:
        yield
    finally:
        if previous_tmp is None:
            os.environ.pop("TMPDIR", None)
        else:
            os.environ["TMPDIR"] = previous_tmp
        os.sched_setaffinity(0, previous_affinity)


def _benchmark_commands(config: BenchmarkConfig) -> str:
    # Each independent command waits for the shell-ready marker. Keep every
    # UART line below the guest's 128-byte input buffer, including its newline.
    lines = ["failed=0", "b=/opt/thekernel-tests/bin/thekernel-kernel-bench",
             f"export KERNEL_BENCH_WORKERS={config.cpus}"]
    if config.suite in {"scheduler", "all"}:
        lines += ["tp=/sys/kernel/tracing",
                  '[ -r "$tp/events/sched/sched_wakeup/id" ] || /bin/busybox mount -t tracefs tracefs "$tp" || failed=1']
    lines += [f'[ "$failed" != 0 ] || "$b" {config.suite} {config.iterations} /root/thekernel-bench.data || failed=1',
              f'[ "$failed" = 0 ] && echo {COMPLETE_MARKER}',
              "/bin/busybox poweroff -f", "exit"]
    if any(len(line.encode("utf-8")) + 1 >= 128 for line in lines):
        raise RunnerError("benchmark command exceeds guest UART line capacity")
    return "\n".join(lines) + "\n"


def run_benchmark_experiment(config: BenchmarkConfig) -> dict:
    """Run fresh guests sequentially; rotate order and discard a warmup round."""
    host_cpus = _validate_config(config)
    config.workdir.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="benchmark-", dir=config.workdir))
    base = directory / "rootfs-base.img"
    observations = {target.name: [] for target in config.targets}
    order = []
    pinning_runs = []
    boot_files = []
    discarded = 1 if config.trials >= 10 else 0
    try:
        # Build outputs may be replaced by another build during a long trial
        # matrix. Validate and run private input copies for this experiment.
        targets = []
        for target in config.targets:
            copies = []
            for kind, source in (("kernel", target.kernel), ("esp", target.esp)):
                destination = directory / f"{target.name}-{kind}.input"
                boot_files.append(destination)
                shutil.copyfile(source, destination)
                copies.append(destination)
            copied = replace(target, kernel=copies[0], esp=copies[1])
            validator = validate_linux_esp_kernel if target.name == "linux" else validate_thekernel_esp_kernel
            validator(copied.kernel, copied.esp)
            targets.append(copied)
        config = replace(config, targets=tuple(targets))
        shutil.copyfile(config.rootfs, base)
        with _execution_environment(host_cpus, directory), open(os.devnull, "wb") as quiet:
            for trial in range(-discarded, config.trials):
                offset = max(trial, 0) % len(config.targets)
                targets = config.targets[offset:] + config.targets[:offset]
                if trial >= 0:
                    order.append([target.name for target in targets])
                for target in targets:
                    phase = "warmup" if trial < 0 else f"trial-{trial:02d}"
                    current = directory / f"{phase}-{target.name}"
                    current.mkdir()
                    rootfs = current / "rootfs.img"
                    command = current / "commands"
                    command.write_text(_benchmark_commands(config), encoding="utf-8")
                    print(f"benchmark {phase} {target.name}: {current / 'console.log'}", file=sys.stderr)
                    try:
                        shutil.copyfile(base, rootfs)
                        with rootfs.open("r+b") as image:
                            os.fsync(image.fileno())
                        result = run(RunConfig(
                            arch="x86_64", kernel=target.kernel, esp=target.esp,
                            rootfs=rootfs, rootfs_transport="drive", rootfs_mode="rw",
                            workdir=current, log_path=current / "console.log",
                            input_path=command, limits=RunLimits(total_timeout_secs=config.timeout),
                            interaction=Interaction(interactive=True, input_after_marker=SHELL_MARKER,
                                                   input_line_after_marker=SHELL_MARKER),
                            memory=config.memory, cpus=config.cpus, accel="kvm",
                            graphics_profile="headless", qemu_binary=config.qemu_binary,
                            qmp=QmpControls(socket=current / "qmp.sock", vcpu_host_cpus=host_cpus[:config.cpus]),
                        ), console_stream=quiet)
                        if not result.guest_clean_shutdown or result.error_message is not None or result.runner_termination_reason is not None:
                            raise RunnerError(f"benchmark guest failed: target={target.name} exit={result.returncode} log={result.log_path}")
                        mapping = result.vcpu_affinity
                        if (len(mapping) != config.cpus
                            or tuple((index, host) for index, _tid, host in mapping) != tuple(enumerate(host_cpus[:config.cpus]))
                            or len({tid for _index, tid, _host in mapping}) != config.cpus):
                            raise RunnerError("benchmark did not confirm the requested per-vCPU affinity")
                        pinning_runs.append({"phase": phase, "target": target.name, "mapping": mapping})
                        rows = parse_benchmark_log(result.log_path, config.suite, config.iterations,
                                                   linux=target.name == "linux", workers=config.cpus)
                        if trial >= 0:
                            observations[target.name].append(rows)
                    finally:
                        rootfs.unlink(missing_ok=True)
    finally:
        base.unlink(missing_ok=True)
        for boot_file in boot_files:
            boot_file.unlink(missing_ok=True)
    comparisons = []
    for target in config.targets:
        if target.name == "baseline":
            continue
        for scenario in sorted(observations["baseline"][0]):
            reference_rows = [trial[scenario] for trial in observations["baseline"]]
            target_rows = [trial[scenario] for trial in observations[target.name]]
            for metric, (_value, smaller) in _metrics(reference_rows[0]).items():
                reference_values = [_metrics(row)[metric][0] for row in reference_rows]
                target_values = [_metrics(row)[metric][0] for row in target_rows]
                comparisons.append({
                    "reference": "baseline", "target": target.name, "scenario": list(scenario),
                    "metric": metric, "smaller_is_better": smaller,
                    "predeclared_primary_target": (
                        config.cpus == 4 and scenario == ("scheduler", "mixed") and metric == "wake_to_run_p99_ns"
                    ) or (
                        scenario == ("io", "random", 4096, 32, "fixed", "buffered", "read", "none")
                        and metric == "elapsed_ns_per_operation"
                    ),
                    "reference_trials": reference_values, "target_trials": target_values,
                    **({"inference": "incomparable_counter_semantics",
                        "reason": "Linux counts pending migration at sched-in; TheKernel counts changed execution CPU"}
                       if target.name == "linux" and metric == "foreground_cpu_migrations"
                       else paired_improvement(reference_values, target_values, smaller_better=smaller)
                       if config.trials >= 10 and all(value > 0 for value in reference_values + target_values)
                       else {"inference": "zero_values_no_ratio" if config.trials >= 10 else "insufficient_trials",
                             "absolute_trial_changes": [target - reference for reference, target in zip(reference_values, target_values)]}),
                })
    return {
        "suite": config.suite, "trials": config.trials, "discarded_warmup_rounds": discarded,
        "acceptance": "measured" if config.trials >= 10 else "smoke_only",
        "iterations": config.iterations, "order": order, "workdir": str(directory),
        "configuration": {"accel": "kvm", "cpus": config.cpus, "memory": config.memory,
                          "host_cpu_mask": list(host_cpus), "per_vcpu_thread_pinning": True,
                          "vcpu_host_cpus": list(host_cpus[:config.cpus]),
                          "rootfs_transport": "drive", "rootfs_mode": "private_raw_rw",
                          "graphics_profile": "headless", "linux_version": "7.2.3"},
        "vcpu_affinity_runs": pinning_runs,
        "comparisons": comparisons, "default_policy_selection": "not_performed",
        "limitations": ["I/O and emulator threads retain the common host CPU mask",
                        "Foreground counters exclude background workers; maxRSS is the process lifetime high-water mark",
                        "Compare fsync_per_batch only at the same queue depth"],
    }
