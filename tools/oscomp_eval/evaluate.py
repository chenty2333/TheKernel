"""Compatibility exports for replay evaluation.

The replay implementation lives in tools.oscomp_eval.replay. This module keeps
older imports working without preserving a second orchestration path.
"""

from __future__ import annotations

from .replay import ReplayRunResult as EvaluateResult
from .replay import evaluate_replay, replay_status as evaluate_run_status
from .replay import score_with_extra_issues

__all__ = [
    "EvaluateResult",
    "evaluate_replay",
    "evaluate_run_status",
    "score_with_extra_issues",
]
