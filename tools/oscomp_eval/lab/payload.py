"""Focused-lab payload generation."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

from tools.build import ensure_support_disk

from ..paths import repo_root
from .model import FocusPlan, PayloadDraft, Selection


def lab_state_root(root: Path | None = None) -> Path:
    return (root or repo_root()) / ".state" / "oscomp-lab"


def plan_key(arch: str, selections: tuple[Selection, ...]) -> str:
    digest = hashlib.sha256()
    digest.update(arch.encode())
    for selection in selections:
        digest.update(b"\0")
        digest.update(selection.text.encode())
    return digest.hexdigest()[:16]


def write_payload(
    *,
    arch: str,
    selections: tuple[Selection, ...],
    draft: PayloadDraft,
    root: Path | None = None,
    materialize: bool = True,
) -> FocusPlan:
    root = root or repo_root()
    key_text = plan_key(arch, selections)
    payload_dir = lab_state_root(root) / "plans" / key_text
    plan_path = payload_dir / "oscomp_plan.txt"
    cases_path = payload_dir / "oscomp_cases.txt"
    ltp_list_path = payload_dir / "ltp_test.txt"
    if materialize:
        payload_dir.mkdir(parents=True, exist_ok=True)
        plan_path.write_text(
            "".join(f"/{libc} {group}\n" for group, libc in draft.group_matrix),
            encoding="utf-8",
        )
        cases_path.write_text(
            "".join(f"{case.group_id} {case.name}\n" for case in draft.cases),
            encoding="utf-8",
        )
        ltp_cases = [case for case in draft.cases if case.group == "ltp"]
        if ltp_cases:
            ltp_list_path.write_text(
                "".join(f"{case.ltp_line}\n" for case in ltp_cases),
                encoding="utf-8",
            )
        else:
            default_ltp = root / "ltp_test.txt"
            ltp_list_path.write_text(default_ltp.read_text(encoding="utf-8"), encoding="utf-8")
        (payload_dir / "lab.json").write_text(
            json.dumps(
                {
                    "arch": arch,
                    "selections": [selection.text for selection in selections],
                    "group_matrix": [
                        {"group": group, "libc": libc} for group, libc in draft.group_matrix
                    ],
                    "cases": [case.__dict__ | {"tags": sorted(case.tags)} for case in draft.cases],
                    "notes": draft.notes,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
    return FocusPlan(
        arch=arch,  # type: ignore[arg-type]
        selections=selections,
        group_matrix=tuple(draft.group_matrix),
        cases=tuple(draft.cases),
        plan_path=plan_path,
        cases_path=cases_path,
        ltp_list_path=ltp_list_path,
        notes=tuple(draft.notes),
    )


def build_focused_support_image(plan: FocusPlan, *, root: Path | None = None) -> Path:
    root = (root or repo_root()).resolve()
    result = ensure_support_disk(
        arch=plan.arch,
        root=root,
        plan=plan.plan_path,
        cases=plan.cases_path,
        ltp_list=plan.ltp_list_path,
    )
    return result.output_path
