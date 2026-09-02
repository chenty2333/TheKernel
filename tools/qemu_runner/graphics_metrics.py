"""Machine-readable graphics benchmark metrics and gate policy.

The guest writes one JSON object per line, prefixed with
``THEKERNEL_GRAPHICS_METRIC ``.  Keeping the format on the serial console
makes it survive failures and lets the normal QEMU run directory remain the
only retained test artifact.
"""

from __future__ import annotations

import json
import math
from dataclasses import asdict, dataclass
from pathlib import Path
from statistics import fmean

from .graphics_benchmark import BENCHMARK_RENDERER_PREFIX


METRIC_PREFIX = "THEKERNEL_GRAPHICS_METRIC "
WARMUP_FRAMES = 60
SAMPLE_FRAMES = 600
INPUT_SAMPLES = 10
COUNTER_RESOURCE_KEYS = frozenset({"schema", "gpu_present", "atomic_commits", "vblanks"})
BACKLOG_RESOURCE_KEYS = frozenset({
    "fences_pending", "pending_atomic_commits", "pending_vblank_events",
    "render_jobs", "render_pending", "present_jobs", "cursor_jobs",
    "retired_2d_resources", "retired_render_resources",
})
ZERO_RESOURCE_KEYS = frozenset({"fences_error", "final_2d_leaks", "final_render_leaks"})


class GraphicsMetricError(ValueError):
    """Raised when a benchmark transcript is incomplete or violates a gate."""


@dataclass(frozen=True)
class GraphicsMetrics:
    renderer: str
    frames: int
    average_fps: float
    frame_p99_ms: float
    frames_over_33_3_ms_percent: float
    input_p99_ms: float
    resource_growth: dict[str, int]
    resource_backlog_growth: dict[str, int]
    resource_final: dict[str, int]
    monotonic_backlogs: tuple[str, ...]

    def json(self) -> str:
        return json.dumps(asdict(self), sort_keys=True)


def _p99(values: list[float]) -> float:
    if not values:
        raise GraphicsMetricError("benchmark did not emit samples")
    return sorted(values)[math.ceil(len(values) * 0.99) - 1]


def parse_graphics_metrics(log_path: Path) -> GraphicsMetrics:
    """Parse and validate the fixed warmup/sample protocol from a QEMU log."""

    frames: list[int] = []
    expected_frame = WARMUP_FRAMES
    input_ms: list[float] = []
    expected_input = 0
    resource_samples: list[dict[str, int]] = []
    renderer: str | None = None
    try:
        lines = log_path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as error:
        raise GraphicsMetricError(f"cannot read graphics benchmark log {log_path}: {error}") from error
    for line in lines:
        if line.startswith(BENCHMARK_RENDERER_PREFIX):
            candidate = line[len(BENCHMARK_RENDERER_PREFIX) :]
            if candidate not in {"software", "virgl", "venus"}:
                raise GraphicsMetricError(f"invalid graphics renderer marker: {candidate}")
            if renderer is not None:
                raise GraphicsMetricError("benchmark emitted more than one graphics renderer marker")
            renderer = candidate
            continue
        if not line.startswith(METRIC_PREFIX):
            continue
        try:
            event = json.loads(line[len(METRIC_PREFIX) :])
        except json.JSONDecodeError as error:
            raise GraphicsMetricError(f"invalid graphics metric JSON: {line}") from error
        kind = event.get("kind")
        if kind == "frame":
            index, ns = event.get("index"), event.get("ns")
            if not isinstance(index, int) or not isinstance(ns, int) or ns <= 0:
                raise GraphicsMetricError("frame metric requires positive integer index and ns")
            if index >= WARMUP_FRAMES:
                if index != expected_frame:
                    raise GraphicsMetricError(
                        f"frame index must be strictly contiguous from {WARMUP_FRAMES}; expected {expected_frame}, got {index}"
                    )
                expected_frame += 1
                frames.append(ns)
        elif kind == "input_to_visible":
            index, ns = event.get("index"), event.get("ns")
            if not isinstance(index, int) or not isinstance(ns, int) or ns < 0:
                raise GraphicsMetricError("input metric requires integer index and non-negative integer ns")
            if index != expected_input:
                raise GraphicsMetricError(
                    f"input index must be strictly contiguous from 0; expected {expected_input}, got {index}"
                )
            expected_input += 1
            input_ms.append(ns / 1_000_000)
        elif kind == "resources":
            values = event.get("values")
            if not isinstance(values, dict) or any(type(v) is not int or v < 0 for v in values.values()):
                raise GraphicsMetricError("resource metric requires non-negative integer values")
            normalized = {str(key): value for key, value in values.items()}
            resource_samples.append(normalized)
    if len(frames) != SAMPLE_FRAMES:
        raise GraphicsMetricError(
            f"benchmark requires exactly {SAMPLE_FRAMES} post-warmup frames, got {len(frames)}"
        )
    if renderer is None:
        raise GraphicsMetricError("benchmark did not emit a graphics renderer marker")
    if len(input_ms) != INPUT_SAMPLES:
        raise GraphicsMetricError(
            f"benchmark requires exactly {INPUT_SAMPLES} host input-to-visible samples, got {len(input_ms)}"
        )
    if len(resource_samples) < 2:
        raise GraphicsMetricError("benchmark requires at least pre- and post-run resource samples")
    resource_keys = resource_samples[0].keys()
    if any(sample.keys() != resource_keys for sample in resource_samples[1:]):
        raise GraphicsMetricError("resource sample key set changed during benchmark")
    growth = {
        key: resource_samples[-1][key] - resource_samples[0][key]
        for key in resource_keys
        if key not in COUNTER_RESOURCE_KEYS | BACKLOG_RESOURCE_KEYS | ZERO_RESOURCE_KEYS
    }
    backlog_growth = {
        key: resource_samples[-1][key] - resource_samples[0][key]
        for key in resource_keys if key in BACKLOG_RESOURCE_KEYS
    }
    monotonic_backlogs = tuple(
        sorted(
            key for key in BACKLOG_RESOURCE_KEYS & resource_keys
            if len(resource_samples) >= 3
            and resource_samples[-1][key] > resource_samples[0][key]
            and all(before[key] <= after[key] for before, after in zip(resource_samples, resource_samples[1:]))
        )
    )
    frame_ms = [value / 1_000_000 for value in frames]
    return GraphicsMetrics(
        renderer=renderer,
        frames=len(frames),
        average_fps=1000 / fmean(frame_ms),
        frame_p99_ms=_p99(frame_ms),
        frames_over_33_3_ms_percent=100 * sum(value > 33.3 for value in frame_ms) / len(frame_ms),
        input_p99_ms=_p99(input_ms),
        resource_growth=growth,
        resource_backlog_growth=backlog_growth,
        resource_final=resource_samples[-1],
        monotonic_backlogs=monotonic_backlogs,
    )


def enforce_graphics_metrics(
    metrics: GraphicsMetrics,
    linux_oracle: GraphicsMetrics | None = None,
    *,
    expected_renderer: str | None = None,
) -> None:
    """Enforce the fixed Q35/KVM acceptance thresholds.

    The caller establishes Linux-host validity first: its absolute gate must
    pass before it can become a relative oracle for TheKernel.
    """

    failures: list[str] = []
    if expected_renderer is not None and metrics.renderer != expected_renderer:
        failures.append(
            f"renderer {metrics.renderer} does not match requested {expected_renderer} topology"
        )
    if metrics.average_fps < 59:
        failures.append(f"average fps {metrics.average_fps:.3f} < 59")
    if metrics.frame_p99_ms > 20:
        failures.append(f"frame p99 {metrics.frame_p99_ms:.3f}ms > 20ms")
    if metrics.frames_over_33_3_ms_percent > 0.1:
        failures.append(f"frames >33.3ms {metrics.frames_over_33_3_ms_percent:.3f}% > 0.1%")
    if metrics.input_p99_ms > 33.3:
        failures.append(f"input p99 {metrics.input_p99_ms:.3f}ms > 33.3ms")
    growing = sorted(key for key, value in metrics.resource_growth.items() if value > 0)
    if growing:
        failures.append("monotonic resource growth: " + ", ".join(growing))
    growing_backlogs = sorted(key for key, value in metrics.resource_backlog_growth.items() if value > 0)
    if growing_backlogs:
        failures.append("resource backlog growth: " + ", ".join(growing_backlogs))
    if metrics.monotonic_backlogs:
        failures.append("monotonic resource backlog: " + ", ".join(metrics.monotonic_backlogs))
    nonzero_terminal = sorted(
        key for key in ZERO_RESOURCE_KEYS
        if metrics.resource_final.get(key, 0) != 0
    )
    if nonzero_terminal:
        failures.append("terminal graphics errors/leaks: " + ", ".join(nonzero_terminal))
    if linux_oracle is not None:
        if metrics.renderer != linux_oracle.renderer:
            failures.append(
                f"renderer differs from Linux oracle ({metrics.renderer} != {linux_oracle.renderer})"
            )
        # The oracle itself has already passed the absolute checks.  It is
        # therefore a valid host only when its values are usable denominators.
        if linux_oracle.average_fps <= 0 or linux_oracle.frame_p99_ms <= 0 or linux_oracle.input_p99_ms <= 0:
            failures.append("invalid Linux oracle metrics")
        else:
            if metrics.average_fps < linux_oracle.average_fps * 0.85:
                failures.append("throughput below 85% of Linux oracle")
            if metrics.frame_p99_ms > linux_oracle.frame_p99_ms * 1.25:
                failures.append("frame p99 exceeds 1.25x Linux oracle")
            if metrics.input_p99_ms > linux_oracle.input_p99_ms * 1.25:
                failures.append("input p99 exceeds 1.25x Linux oracle")
    if failures:
        raise GraphicsMetricError("graphics benchmark gate failed: " + "; ".join(failures))
