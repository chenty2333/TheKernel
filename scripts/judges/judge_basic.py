#!/usr/bin/env python3

import json
import os
import re
import subprocess
import sys
from pathlib import Path


DEFAULT_BASE_JUDGE = Path.home() / "T202510003995291-2331" / "apps" / "oscomp" / "judge_basic.py"
SECTION_RE = re.compile(
    r"========== START (?P<name>.+?) ==========\n(?P<body>.*?)========== END .*? ==========",
    re.S,
)


def load_sections(text: str) -> dict[str, list[str]]:
    sections: dict[str, list[str]] = {}
    normalized = text.replace("\r\n", "\n").replace("\r", "")
    for match in SECTION_RE.finditer(normalized):
        name = match.group("name").strip()
        body = [line for line in match.group("body").splitlines() if line.strip()]
        sections[name] = body
    return sections


def run_base_judge(text: str) -> list[dict]:
    judge_path = Path(os.environ.get("OSCOMP_BASIC_BASE_JUDGE", DEFAULT_BASE_JUDGE))
    if not judge_path.is_file():
        raise FileNotFoundError(f"base basic judge not found: {judge_path}")

    proc = subprocess.run(
        [sys.executable, str(judge_path)],
        input=text,
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.stderr:
        sys.stderr.write(proc.stderr)
    if proc.returncode != 0:
        raise RuntimeError(f"base judge exited with {proc.returncode}")

    json_line = None
    for line in reversed(proc.stdout.splitlines()):
        line = line.strip()
        if line.startswith("[") and line.endswith("]"):
            json_line = line
            break
    if json_line is None:
        raise RuntimeError("base judge did not emit JSON")
    return json.loads(json_line)


def patch_basic_102(results: list[dict], sections: dict[str, list[str]]) -> list[dict]:
    by_name = {item["name"]: item for item in results}

    sleep = by_name.get("test_sleep")
    if sleep is not None:
        sleep["all"] = 2
        sleep["pass"] = 2 if sections.get("test_sleep") == ["sleep success."] else 0
        sleep["score"] = sleep["pass"]

    pipe = by_name.get("test_pipe")
    if pipe is not None:
        lines = sections.get("test_pipe", [])
        has_child_zero = any(line == "cpid: 0" for line in lines)
        has_child_pid = any(re.fullmatch(r"cpid: [1-9]\d*", line) for line in lines)
        has_write_ok = any(line == "  Write to pipe successfully." for line in lines)
        pipe["all"] = 4
        pipe["pass"] = int(has_child_zero) + int(has_child_pid) + 2 * int(has_write_ok)
        pipe["score"] = pipe["pass"]

    return results


def main() -> int:
    text = sys.stdin.read()
    sections = load_sections(text)
    results = run_base_judge(text)
    results = patch_basic_102(results, sections)
    print(json.dumps(results))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
