#!/usr/bin/env python3
"""Queue hardware verification only for an available KVM runner; an explicit request that cannot be served is an error."""
import json
import os
import subprocess
from pathlib import Path

REQUIRED = {"self-hosted", "linux", "x64", "thekernel-kvm"}
# A scheduled run asks for KVM opportunistically: no KVM runner being online at
# 03:17 is not a repository finding, so the probe reports the skip and succeeds.
# An explicit `tier=hardware` dispatch is a request for that evidence, and a
# missing runner has to fail visibly instead of printing a green no-op.
OPTIONAL_ENVIRONMENT = "THEKERNEL_HARDWARE_OPTIONAL"


def available(pages):
    return any(runner.get("status") == "online" and not runner.get("busy", True)
               and REQUIRED <= {label["name"].lower() for label in runner.get("labels", [])}
               for page in pages for runner in page.get("runners", []))


def main():
    repository = os.environ["GITHUB_REPOSITORY"]
    try:
        result = subprocess.run(["gh", "api", "--paginate", "--slurp", f"repos/{repository}/actions/runners?per_page=100"],
                                capture_output=True, text=True, check=False, timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        result = subprocess.CompletedProcess([], 1, "", "")
    try:
        ready = result.returncode == 0 and available(json.loads(result.stdout))
    except (ValueError, TypeError, KeyError):
        ready = False
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"available={'true' if ready else 'false'}\n")
    if ready:
        print("hardware: runner available; scheduling explicit KVM verification")
        return 0
    reason = "runner inventory inaccessible (requires Administration:read token)" if result.returncode else "no online idle runner with self-hosted/linux/x64/thekernel-kvm labels"
    if os.environ.get(OPTIONAL_ENVIRONMENT) == "1":
        message = f"hardware: skipped, {reason}"
        print(message)
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
            summary.write(message + "\n")
        return 0
    message = f"hardware: NOT RUN type=environment-unavailable: {reason}"
    print(f"::error::{message}")
    with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a") as summary:
        summary.write(message + "\n")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
