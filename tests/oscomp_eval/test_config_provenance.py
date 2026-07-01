from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.oscomp_eval.config import (
    ConfigError,
    EvalConfig,
    canonical_arch,
    expand_arches,
    expand_expected_matrix,
    group_libc_matrix_from_plan_text,
)
from tools.oscomp_eval.paths import official_judge_dir, prepare_run_dir, repo_root
from tools.oscomp_eval.provenance import (
    load_official_snapshot,
    refresh_official_snapshot,
)


class ConfigTests(unittest.TestCase):
    def test_arch_aliases_are_canonicalized(self) -> None:
        self.assertEqual(canonical_arch("riscv64"), "rv")
        self.assertEqual(canonical_arch("loongarch64"), "la")
        with self.assertRaises(ConfigError):
            canonical_arch("x86_64")

    def test_both_arches_expand_to_current_default_order(self) -> None:
        self.assertEqual(expand_arches("both"), ("rv", "la"))
        self.assertEqual(expand_arches(["rv", "loongarch64"]), ("rv", "la"))

    def test_default_matrix_matches_current_local_plan(self) -> None:
        matrix = expand_expected_matrix()
        keys = [cell.key for cell in matrix]
        self.assertEqual(len(matrix), 42)
        self.assertIn("rv/basic-musl", keys)
        self.assertIn("la/ltp-glibc", keys)
        self.assertNotIn("rv/libctest-glibc", keys)

    def test_eval_config_serializes_without_dataclass_leakage(self) -> None:
        data = EvalConfig(arches=("rv",)).to_json_dict()
        self.assertEqual(data["arches"], ["rv"])
        self.assertIsInstance(data["group_libc_matrix"], list)

    def test_group_libc_matrix_from_plan_text_matches_guest_plan_shape(self) -> None:
        matrix = group_libc_matrix_from_plan_text(
            "# focused plan\n"
            "/musl basic\n"
            "/glibc basic\n"
            "lua-musl\n"
            "lua-musl\n"
        )

        self.assertEqual(
            matrix,
            (
                ("basic", "musl"),
                ("basic", "glibc"),
                ("lua", "musl"),
            ),
        )


class PathAndProvenanceTests(unittest.TestCase):
    def test_repo_root_and_official_judge_dir_exist(self) -> None:
        root = repo_root()
        self.assertTrue((root / "scripts" / "oscomp.sh").is_file())
        self.assertTrue((official_judge_dir(root) / "judge_basic-musl.py").is_file())

    def test_prepare_run_dir_replace_clears_only_known_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = Path(tmp) / "run"
            (run_dir / "rv").mkdir(parents=True)
            (run_dir / "rv" / "stale.log").write_text("stale\n", encoding="utf-8")
            (run_dir / "manifest.json").write_text("{}\n", encoding="utf-8")
            (run_dir / "keep.txt").write_text("keep\n", encoding="utf-8")

            prepared = prepare_run_dir(run_dir, replace=True)

            self.assertEqual(prepared, run_dir)
            self.assertFalse((run_dir / "rv").exists())
            self.assertFalse((run_dir / "manifest.json").exists())
            self.assertEqual((run_dir / "keep.txt").read_text(encoding="utf-8"), "keep\n")

    def test_official_manifest_records_snapshot(self) -> None:
        snapshot = load_official_snapshot()
        self.assertEqual(snapshot.schema, "oscomp-eval.official-snapshot.v1")
        self.assertEqual(
            snapshot.commit,
            "d1bb3a3c4b27274e196a2648518525c1a304e339",
        )
        self.assertIn("judge/judge_basic-musl.py", snapshot.files)
        self.assertEqual(snapshot.local_patches, ())

    def test_refresh_official_snapshot_from_explicit_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "repo"
            source = Path(tmp) / "official-src"
            source_judge = source / "kernel" / "judge"
            dest = root / "tools" / "oscomp_eval" / "official"
            source_judge.mkdir(parents=True)
            (source_judge / "config.json").write_text('{"ok": true}\n', encoding="utf-8")
            (source_judge / "judge_basic-musl.py").write_text("print('new')\n", encoding="utf-8")
            (source_judge / "judge_lua-glibc.py").write_text("print('lua')\n", encoding="utf-8")
            (source / "LICENSE").write_text("license text\n", encoding="utf-8")
            (dest / "judge").mkdir(parents=True)
            (dest / "judge" / "judge_basic-musl.py").write_text("print('old')\n", encoding="utf-8")
            (dest / "judge" / "judge_old-musl.py").write_text("print('old')\n", encoding="utf-8")
            (dest / "manifest.json").write_text(
                json.dumps(
                    {
                        "schema": "oscomp-eval.official-snapshot.v1",
                        "source": {
                            "repo": "old",
                            "commit": "old",
                            "source_path": "old",
                            "imported_at": "old",
                            "license_note": "old",
                        },
                        "files": [
                            "judge/judge_basic-musl.py",
                            "judge/judge_old-musl.py",
                        ],
                        "local_patches": [],
                    }
                ),
                encoding="utf-8",
            )

            snapshot = refresh_official_snapshot(
                source,
                root=root,
                repo="https://example.invalid/autotest.git",
                commit="abc123",
                imported_at="2026-07-01",
            )

            self.assertEqual(snapshot.repo, "https://example.invalid/autotest.git")
            self.assertEqual(snapshot.commit, "abc123")
            self.assertEqual(snapshot.source_status, "unknown")
            self.assertIn("judge/config.json", snapshot.files)
            self.assertIn("judge/judge_basic-musl.py", snapshot.files)
            self.assertIn("judge/judge_lua-glibc.py", snapshot.files)
            self.assertFalse((dest / "judge" / "judge_old-musl.py").exists())
            self.assertEqual(
                (dest / "judge" / "judge_basic-musl.py").read_text(encoding="utf-8"),
                "print('new')\n",
            )
            self.assertEqual(
                snapshot.changes,
                {
                    "added": ("judge/config.json", "judge/judge_lua-glibc.py"),
                    "removed": ("judge/judge_old-musl.py",),
                    "changed": ("judge/judge_basic-musl.py",),
                },
            )
            loaded = load_official_snapshot(dest / "manifest.json")
            self.assertEqual(loaded.changes, snapshot.changes)
            self.assertIn("LICENSE", loaded.license_note)


if __name__ == "__main__":
    unittest.main()
