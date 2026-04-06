#!/usr/bin/env python3

import argparse
import json
import pathlib
import subprocess
import sys


SCRIPT_BY_GROUP = {
    "basic": "judge_basic.py",
    "busybox": "judge_busybox.py",
    "lua": "judge_lua.py",
    "libctest": "judge_libctest.py",
}

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
LOCAL_JUDGE_DIR = REPO_ROOT / "scripts" / "judges"


def extract_group_output(log_text: str, group: str, root: str) -> str:
    start = f"#### OS COMP TEST GROUP START {group}-{root} ####"
    end = f"#### OS COMP TEST GROUP END {group}-{root} ####"

    start_idx = log_text.find(start)
    if start_idx < 0:
        raise ValueError(f"missing start marker: {start}")

    end_idx = log_text.find(end, start_idx)
    if end_idx < 0:
        raise ValueError(f"missing end marker: {end}")

    body_start = start_idx + len(start)
    body = log_text[body_start:end_idx]
    body = body.replace("\r\n", "\n").replace("\r", "")
    return body.lstrip("\n")


def summarize(results: list[dict]) -> tuple[float, float]:
    score = 0.0
    total = 0.0
    for item in results:
        score += float(item.get("score", 0))
        if "all" in item:
            total += float(item["all"])
        else:
            total += 1.0
    return score, total


def main() -> int:
    parser = argparse.ArgumentParser(description="Judge a single OSCOMP group from a QEMU log.")
    parser.add_argument("--log", required=True, type=pathlib.Path)
    parser.add_argument("--group", required=True)
    parser.add_argument("--root", required=True, choices=["musl", "glibc"])
    parser.add_argument("--judge-dir", required=True, type=pathlib.Path)
    parser.add_argument("--json-out", type=pathlib.Path)
    args = parser.parse_args()

    judge_name = SCRIPT_BY_GROUP.get(args.group)
    if judge_name is None:
        print(f"[judge-oscomp] skip: unsupported group {args.group}", file=sys.stderr)
        return 2

    judge_script = LOCAL_JUDGE_DIR / judge_name
    if not judge_script.is_file():
        judge_script = args.judge_dir / judge_name
    if not judge_script.is_file():
        print(f"[judge-oscomp] skip: judge script not found: {judge_script}", file=sys.stderr)
        return 2

    log_text = args.log.read_text(encoding="utf-8", errors="replace")
    try:
        group_output = extract_group_output(log_text, args.group, args.root)
    except ValueError as exc:
        print(f"[judge-oscomp] skip: {exc}", file=sys.stderr)
        return 2

    proc = subprocess.run(
        [sys.executable, str(judge_script)],
        input=group_output,
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.stderr:
        sys.stderr.write(proc.stderr)
    if proc.returncode != 0:
        print(
            f"[judge-oscomp] error: judge script exited with {proc.returncode}",
            file=sys.stderr,
        )
        if proc.stdout:
            sys.stderr.write(proc.stdout)
        return proc.returncode

    payload = proc.stdout.strip()
    if not payload:
        print("[judge-oscomp] error: empty judge output", file=sys.stderr)
        return 1

    results = json.loads(payload)
    score, total = summarize(results)

    if args.json_out:
        args.json_out.write_text(json.dumps(results, ensure_ascii=True, indent=2) + "\n")

    print(f"[judge-oscomp] {args.group}-{args.root}: score {score:.0f}/{total:.0f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
