#!/usr/bin/env python3

from __future__ import annotations

import csv
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PARSER = REPO_ROOT / "scripts" / "ci" / "parse-mm-lock-diagnostics.py"
STAGES = (
    "user_pin_admission",
    "user_pin_expectation",
    "user_pin_collect_owners",
    "user_pin_revalidate",
    "user_pin_commit",
    "user_pin_release",
    "mremap_optimistic_plan",
    "mremap_optimistic_commit",
    "mremap_serialized",
    "phys_pin_registry_shard",
    "phys_pin_publish_shard",
    "phys_pin_release_shard",
    "phys_pin_dealloc_probe_shard",
)
REQUIRED_EXERCISED_STAGES = (
    "user_pin_admission",
    "user_pin_expectation",
    "user_pin_collect_owners",
    "user_pin_revalidate",
    "user_pin_commit",
    "user_pin_release",
    "phys_pin_publish_shard",
    "phys_pin_release_shard",
)
MREMAP_STAGES = (
    "mremap_optimistic_plan",
    "mremap_optimistic_commit",
    "mremap_serialized",
)
U64_MAX = (1 << 64) - 1


def histogram(bucket: int = 4, count: int = 1) -> str:
    values = [0] * 64
    values[bucket] = count
    return ",".join(str(value) for value in values)


def valid_log(*, zero_stages: frozenset[str] = frozenset()) -> str:
    lines = [
        "MM_LOCK_DIAGNOSTICS schema=thekernel-mm-lock-diagnostics-v1 "
        "enabled=0 resetting=0 active_samples=0 epoch=7 sequence=91 "
        "sequence_exhausted=0 histogram=log2_ns_v1"
    ]
    for stage in STAGES:
        if stage in zero_stages:
            lines.append(
                f"MM_LOCK_STAGE stage={stage} epoch=7 samples=0 "
                "wait_sum_ns=0 wait_max_ns=0 hold_sum_ns=0 hold_max_ns=0 "
                f"saturated=0 wait_buckets={histogram(count=0)} "
                f"hold_buckets={histogram(count=0)}"
            )
        else:
            lines.append(
                f"MM_LOCK_STAGE stage={stage} epoch=7 samples=1 "
                "wait_sum_ns=9 wait_max_ns=9 hold_sum_ns=11 hold_max_ns=11 "
                f"saturated=0 wait_buckets={histogram()} "
                f"hold_buckets={histogram()}"
            )
    lines.append(
        "MM_LOCK_DIAGNOSTICS_END enabled=0 resetting=0 active_samples=0 epoch=7 "
        "sequence=91 sequence_exhausted=0"
    )
    return "\n".join(lines) + "\n"


def replace_stage_fields(text: str, stage: str, **updates: str | int) -> str:
    lines = text.splitlines()
    prefix = f"MM_LOCK_STAGE stage={stage} "
    index = next(index for index, line in enumerate(lines) if line.startswith(prefix))
    fields = lines[index].split(" ")
    positions = {token.split("=", 1)[0]: offset for offset, token in enumerate(fields)}
    for key, value in updates.items():
        fields[positions[key]] = f"{key}={value}"
    lines[index] = " ".join(fields)
    return "\n".join(lines) + "\n"


class ParserTests(unittest.TestCase):
    def run_parser(self, text: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "qemu.log"
            log.write_text(text, encoding="ascii")
            return subprocess.run(
                ["python3", str(PARSER), str(log)],
                cwd=REPO_ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )

    def test_normalizes_complete_disabled_snapshot(self) -> None:
        result = self.run_parser(valid_log())
        self.assertEqual(result.returncode, 0, msg=result.stderr)
        rows = list(csv.DictReader(result.stdout.splitlines(), delimiter="\t"))
        self.assertEqual([row["stage"] for row in rows], list(STAGES))
        self.assertTrue(all(row["wait_p99_upper_ns"] == "15" for row in rows))
        self.assertTrue(all(row["hold_p999_upper_ns"] == "15" for row in rows))

    def test_accepts_unexercised_optional_registry_stage(self) -> None:
        result = self.run_parser(
            valid_log(zero_stages=frozenset({"phys_pin_registry_shard"}))
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)

    def test_rejects_enabled_or_incomplete_snapshot(self) -> None:
        enabled = valid_log().replace("enabled=0", "enabled=1", 1)
        self.assertEqual(self.run_parser(enabled).returncode, 2)
        resetting = valid_log().replace("resetting=0", "resetting=1", 1)
        self.assertEqual(self.run_parser(resetting).returncode, 2)
        active = valid_log().replace("active_samples=0", "active_samples=1", 1)
        self.assertEqual(self.run_parser(active).returncode, 2)
        exhausted = valid_log().replace(
            "sequence_exhausted=0", "sequence_exhausted=1"
        )
        self.assertEqual(self.run_parser(exhausted).returncode, 2)
        lying_exhausted = valid_log().replace("sequence=91", f"sequence={U64_MAX}")
        self.assertEqual(self.run_parser(lying_exhausted).returncode, 2)
        incomplete = "\n".join(
            line
            for line in valid_log().splitlines()
            if f"stage={STAGES[-1]} " not in line
        ) + "\n"
        self.assertEqual(self.run_parser(incomplete).returncode, 2)

    def test_rejects_epoch_saturation_and_histogram_drift(self) -> None:
        epoch = valid_log().replace("stage=user_pin_admission epoch=7", "stage=user_pin_admission epoch=8", 1)
        self.assertEqual(self.run_parser(epoch).returncode, 2)
        saturated = valid_log().replace("saturated=0", "saturated=1", 1)
        self.assertEqual(self.run_parser(saturated).returncode, 2)
        drift = valid_log().replace("samples=1", "samples=2", 1)
        self.assertEqual(self.run_parser(drift).returncode, 2)

    def test_rejects_maximum_outside_highest_populated_bucket(self) -> None:
        for field, maximum in (("wait_max_ns", 7), ("hold_max_ns", 16)):
            with self.subTest(field=field, maximum=maximum):
                updates = {field: maximum}
                if field == "hold_max_ns":
                    updates["hold_sum_ns"] = maximum
                result = self.run_parser(
                    replace_stage_fields(
                        valid_log(), "user_pin_admission", **updates
                    )
                )
                self.assertEqual(result.returncode, 2)
                self.assertIn("is incompatible with highest populated", result.stderr)

    def test_rejects_sum_outside_histogram_derived_bounds(self) -> None:
        two_samples = {
            "samples": 2,
            "wait_sum_ns": 23,
            "wait_max_ns": 15,
            "hold_sum_ns": 23,
            "hold_max_ns": 15,
            "wait_buckets": histogram(count=2),
            "hold_buckets": histogram(count=2),
        }
        for field, total in (("wait_sum_ns", 15), ("hold_sum_ns", 31)):
            with self.subTest(field=field, total=total):
                result = self.run_parser(
                    replace_stage_fields(
                        valid_log(),
                        "user_pin_admission",
                        **{**two_samples, field: total},
                    )
                )
                self.assertEqual(result.returncode, 2)
                self.assertIn("is outside histogram-derived bounds", result.stderr)

    def test_rejects_sum_incompatible_with_recorded_maximum(self) -> None:
        result = self.run_parser(
            replace_stage_fields(
                valid_log(),
                "user_pin_admission",
                samples=2,
                wait_sum_ns=20,
                wait_max_ns=9,
                wait_buckets=histogram(count=2),
                hold_sum_ns=22,
                hold_max_ns=11,
                hold_buckets=histogram(count=2),
            )
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("wait_sum_ns is outside histogram-derived bounds", result.stderr)

    def test_accepts_bucket_zero_and_u64_last_bucket_boundaries(self) -> None:
        result = self.run_parser(
            replace_stage_fields(
                valid_log(),
                "user_pin_admission",
                wait_sum_ns=0,
                wait_max_ns=0,
                wait_buckets=histogram(bucket=0),
                hold_sum_ns=U64_MAX,
                hold_max_ns=U64_MAX,
                hold_buckets=histogram(bucket=63),
            )
        )
        self.assertEqual(result.returncode, 0, msg=result.stderr)

    def test_rejects_out_of_range_and_implicitly_saturated_u64_totals(self) -> None:
        out_of_range = self.run_parser(
            replace_stage_fields(
                valid_log(),
                "user_pin_admission",
                wait_sum_ns=U64_MAX + 1,
            )
        )
        self.assertEqual(out_of_range.returncode, 2)
        self.assertIn("out-of-range wait_sum_ns", out_of_range.stderr)

        implicitly_saturated = self.run_parser(
            replace_stage_fields(
                valid_log(),
                "user_pin_admission",
                samples=4,
                wait_sum_ns=U64_MAX,
                wait_max_ns=1 << 62,
                wait_buckets=histogram(bucket=63, count=4),
                hold_sum_ns=4,
                hold_max_ns=1,
                hold_buckets=histogram(bucket=1, count=4),
            )
        )
        self.assertEqual(implicitly_saturated.returncode, 2)
        self.assertIn("require a saturated cumulative total", implicitly_saturated.stderr)

    def test_rejects_missing_duplicate_or_torn_end_record(self) -> None:
        lines = valid_log().splitlines()
        missing = "\n".join(lines[:-1]) + "\n"
        result = self.run_parser(missing)
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing MM_LOCK_DIAGNOSTICS_END", result.stderr)

        duplicate = valid_log() + lines[-1] + "\n"
        result = self.run_parser(duplicate)
        self.assertEqual(result.returncode, 2)
        self.assertIn("duplicate MM_LOCK_DIAGNOSTICS_END", result.stderr)

        for field, value in (
            ("enabled", "1"),
            ("resetting", "1"),
            ("active_samples", "1"),
            ("epoch", "8"),
            ("sequence", "92"),
            ("sequence_exhausted", "1"),
        ):
            with self.subTest(field=field):
                torn_lines = list(lines)
                torn_lines[-1] = torn_lines[-1].replace(
                    (
                        f"{field}=0"
                        if field not in {"epoch", "sequence"}
                        else ("epoch=7" if field == "epoch" else "sequence=91")
                    ),
                    f"{field}={value}",
                )
                result = self.run_parser("\n".join(torn_lines) + "\n")
                self.assertEqual(result.returncode, 2)
                self.assertIn("control state changed during read", result.stderr)

    def test_rejects_all_zero_and_each_unexercised_required_stage(self) -> None:
        all_zero = self.run_parser(valid_log(zero_stages=frozenset(STAGES)))
        self.assertEqual(all_zero.returncode, 2)
        self.assertIn("required MM lock stages have no samples", all_zero.stderr)

        for stage in REQUIRED_EXERCISED_STAGES:
            with self.subTest(stage=stage):
                result = self.run_parser(valid_log(zero_stages=frozenset({stage})))
                self.assertEqual(result.returncode, 2)
                self.assertIn(stage, result.stderr)

                missing = "\n".join(
                    line
                    for line in valid_log().splitlines()
                    if f"stage={stage} " not in line
                ) + "\n"
                result = self.run_parser(missing)
                self.assertEqual(result.returncode, 2)
                self.assertIn(stage, result.stderr)

    def test_requires_at_least_one_exercised_mremap_stage(self) -> None:
        result = self.run_parser(valid_log(zero_stages=frozenset(MREMAP_STAGES)))
        self.assertEqual(result.returncode, 2)
        self.assertIn("mremap MM lock stages have no samples", result.stderr)

        for stage in MREMAP_STAGES:
            with self.subTest(stage=stage):
                remaining_zero = frozenset(set(MREMAP_STAGES) - {stage})
                self.assertEqual(
                    self.run_parser(valid_log(zero_stages=remaining_zero)).returncode,
                    0,
                )


if __name__ == "__main__":
    unittest.main()
