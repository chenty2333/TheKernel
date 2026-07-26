#!/usr/bin/env python3

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PARSER = REPO_ROOT / "scripts" / "ci" / "parse-pmu-capabilities.py"
EVENTS = (
    "cpu_cycles",
    "instructions",
    "dtlb_read_misses",
    "dtlb_write_misses",
    "itlb_read_misses",
)


def valid_log(source: str = "sbi-pmu") -> str:
    records = [
        "PMU_CAPABILITIES schema=thekernel-pmu-capabilities-v1 "
        f"source={source} counter_count=2 consistent_snapshot=0 "
        "samples_collected=0"
    ]
    records.extend(
        f"PMU_EVENT event={event} requestable=1 sampled=0" for event in EVENTS
    )
    return "boot noise\n" + "\n".join(records) + "\nshutdown noise\n"


class PmuCapabilitiesParserTests(unittest.TestCase):
    def run_parser(self, payload: str, arch: str = "rv") -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "qemu.log"
            log.write_text(payload, encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(PARSER), str(log), "--arch", arch],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )

    def test_accepts_five_unique_capability_only_events(self) -> None:
        result = self.run_parser(valid_log())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("thekernel-pmu-capabilities-v1\tsbi-pmu\t2\t0", result.stdout)
        self.assertIn("itlb_read_misses\t1\t0", result.stdout)

    def test_rejects_any_claimed_sample(self) -> None:
        header = self.run_parser(valid_log().replace("samples_collected=0", "samples_collected=1"))
        self.assertEqual(header.returncode, 1)
        self.assertIn("samples_collected=0", header.stderr)

        event = self.run_parser(valid_log().replace("sampled=0", "sampled=1", 1))
        self.assertEqual(event.returncode, 1)
        self.assertIn("sampled=0", event.stderr)

    def test_rejects_duplicate_missing_or_wrong_arch_source(self) -> None:
        duplicate_line = "PMU_EVENT event=cpu_cycles requestable=1 sampled=0\n"
        duplicate = self.run_parser(valid_log() + duplicate_line)
        self.assertEqual(duplicate.returncode, 1)
        self.assertIn("duplicate PMU event", duplicate.stderr)

        missing = self.run_parser(
            valid_log().replace(
                "PMU_EVENT event=itlb_read_misses requestable=1 sampled=0\n", ""
            )
        )
        self.assertEqual(missing.returncode, 1)
        self.assertIn("event set mismatch", missing.stderr)

        source = self.run_parser(valid_log(source="loongarch-pmcfg"), arch="rv")
        self.assertEqual(source.returncode, 1)
        self.assertIn("source mismatch", source.stderr)


if __name__ == "__main__":
    unittest.main()
