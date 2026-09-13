"""Unit tests for the observation channels of the hardware DUT gate.

The parked serial contract is covered by ``test_panther_lake_dut_gate``; this
module covers the transport-neutral gate: the network channel, the screen
channel a serial-less DUT needs, and the fail-closed rules that keep either
from passing on evidence the hook did not actually produce.
"""

from __future__ import annotations

import os
import subprocess
import sys
import textwrap
import unittest
from pathlib import Path
from unittest import mock

from tests.support import load_script_module, repo_root, test_tmpdir


def load_gate():
    return load_script_module("thekernel_dut_gate", "scripts/ci/dut_gate.py")


#: Writes the frame set and verdict a screen capture hook would leave behind.
#: ``--mode`` selects one deliberate defect so each validator rule can fail.
FRAME_WRITER = textwrap.dedent(
    """
    import argparse
    from pathlib import Path

    def frame(width, height, lit):
        header = f"P6\\n{width} {height}\\n255\\n".encode()
        body = bytearray(b"\\x00\\x00\\x00" * (width * height))
        for index in lit:
            body[index * 3 : index * 3 + 3] = b"\\xff\\xff\\xff"
        return header + bytes(body)

    parser = argparse.ArgumentParser()
    parser.add_argument("--dir", required=True)
    parser.add_argument("--verdict", required=True)
    parser.add_argument("--mode", required=True)
    parser.add_argument("--dut", required=True)
    parser.add_argument("--run", required=True)
    args = parser.parse_args()

    directory = Path(args.dir)
    directory.mkdir(parents=True, exist_ok=True)
    width, height = 8, 4
    lit = [0, 1, 9, 17, 31]
    files = {"frame-0000.ppm": frame(width, height, lit),
             "frame-0001.ppm": frame(width, height, lit),
             "frame-0002.ppm": frame(width, height, lit)}
    if args.mode == "cursor":
        files = {"frame-0000.ppm": frame(64, 64, [0]),
                 "frame-0001.ppm": frame(64, 64, [0]),
                 "frame-0002.ppm": frame(64, 64, [0])}
    if args.mode == "blank":
        files["frame-0001.ppm"] = frame(width, height, [])  # the completion frame
    if args.mode == "truncated":
        files["frame-0002.ppm"] = files["frame-0002.ppm"][:-4]
    if args.mode == "not_ppm":
        files["frame-0002.ppm"] = b"P3\\n8 4\\n255\\n" + b"0 0 0\\n" * 32
    if args.mode == "gap":
        del files["frame-0001.ppm"]
    for name, body in files.items():
        (directory / name).write_bytes(body)

    verdict = {
        "version": "1",
        "dut": args.dut,
        "run": args.run,
        "resolution": "8x4",
        "frames": str(len(files)),
        "completion_frame": "frame-0001.ppm",
        "last_frame": "frame-0002.ppm",
        "checker": "test-frame-writer/1",
    }
    if args.mode == "cursor":
        verdict["resolution"] = "64x64"
    if args.mode == "missing_completion":
        verdict["completion_frame"] = "frame-0003.ppm"
    if args.mode == "completion_last":
        verdict["completion_frame"] = "frame-0002.ppm"
    if args.mode == "wrong_dut":
        verdict["dut"] = "somebody-else"
    if args.mode == "wrong_run":
        verdict["run"] = "99"
    if args.mode == "version":
        verdict["version"] = "2"
    if args.mode == "missing_key":
        del verdict["checker"]
    if args.mode == "blank_checker":
        verdict["checker"] = ""
    if args.mode == "resolution":
        verdict["resolution"] = "1920x1080"
    if args.mode == "too_few":
        verdict["frames"] = "1"
    if args.mode == "count":
        verdict["frames"] = "7"
    if args.mode == "escape":
        verdict["last_frame"] = "../../etc/passwd"
    if args.mode == "extra":
        verdict["unknown_key"] = "ignored"
    body = "".join(f"{key}={value}\\n" for key, value in verdict.items())
    if args.mode == "malformed":
        body = body.replace("frames=", "frames ", 1)
    if args.mode == "duplicate":
        body += "frames=3\\n"
    Path(args.verdict).write_text(body, encoding="utf-8")
    """
)


class DutGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = test_tmpdir()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.artifact_dir = self.root / "artifact"
        self.artifact_dir.mkdir()
        for name in load_gate().REQUIRED_ARTIFACTS:
            (self.artifact_dir / name).write_bytes(b"product")
        self.state_dir = self.root / "state"
        self.writer = self.root / "write_frames.py"
        self.writer.write_text(FRAME_WRITER, encoding="utf-8")

    def run_with(self, gate, hooks: dict[str, str], **kwargs):
        with mock.patch.dict(os.environ, hooks, clear=False):
            return gate.run_gate(self.artifact_dir, self.state_dir, **kwargs)

    def screen_hooks(self, mode: str, dut: str = "n305") -> dict[str, str]:
        capture = (
            f"{sys.executable} {self.writer} --dir \"$THEKERNEL_DUT_SCREEN_DIR\" "
            f"--verdict \"$THEKERNEL_DUT_SCREEN_VERDICT\" --mode {mode} --dut {dut} "
            f'--run "$THEKERNEL_DUT_RUN"'
        )
        return {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_SCREEN_CAPTURE_CMD": capture,
        }

    def test_screen_channel_accepts_well_formed_evidence(self) -> None:
        gate = load_gate()
        evidence = self.run_with(gate, self.screen_hooks("valid"), dut="n305", runs=1)
        self.assertEqual(len(evidence), 1)
        self.assertEqual(evidence[0].channel, "screen")
        self.assertEqual(evidence[0].facts["frames"], "3")
        self.assertEqual(evidence[0].facts["completion_frame"], "frame-0001.ppm")

    def test_screen_channel_is_the_n305_default(self) -> None:
        gate = load_gate()
        self.assertEqual(gate.PROFILES["n305"].observation, "screen")
        self.assertEqual(gate.PROFILES["panther-lake"].observation, "serial")
        evidence = self.run_with(gate, self.screen_hooks("valid"), dut="n305", runs=2)
        self.assertEqual([item.run for item in evidence], [1, 2])

    def test_screen_channel_ignores_unrelated_files_in_the_frame_directory(self) -> None:
        gate = load_gate()
        hooks = self.screen_hooks("valid")
        hooks["THEKERNEL_DUT_SCREEN_CAPTURE_CMD"] += (
            '; printf notes > "$THEKERNEL_DUT_SCREEN_DIR/notes.txt"'
        )
        self.run_with(gate, hooks, dut="n305", runs=1)

    def test_a_dark_last_frame_is_accepted_as_a_power_off(self) -> None:
        # A DUT that switches itself off after the banner leaves a uniform black
        # screen; that is a successful run, so the last frame may be blank.
        gate = load_gate()
        hooks = self.screen_hooks("valid")
        hooks["THEKERNEL_DUT_SCREEN_CAPTURE_CMD"] += (
            '; printf "P6\\n8 4\\n255\\n" > "$THEKERNEL_DUT_SCREEN_DIR/frame-0002.ppm"'
            '; dd if=/dev/zero bs=96 count=1 status=none >> "$THEKERNEL_DUT_SCREEN_DIR/frame-0002.ppm"'
        )
        self.run_with(gate, hooks, dut="n305", runs=1)

    def test_screen_channel_accepts_unknown_verdict_keys(self) -> None:
        gate = load_gate()
        self.run_with(gate, self.screen_hooks("extra"), dut="n305", runs=1)

    def test_a_blank_screen_never_passes(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "blank"):
            self.run_with(gate, self.screen_hooks("blank"), dut="n305", runs=1)

    def test_a_single_lit_pixel_is_still_a_blank_screen(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "blank"):
            self.run_with(gate, self.screen_hooks("cursor"), dut="n305", runs=1)

    def test_screen_channel_rejects_a_truncated_frame(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "truncated"):
            self.run_with(gate, self.screen_hooks("truncated"), dut="n305", runs=1)

    def test_screen_channel_rejects_a_frame_that_is_not_a_binary_ppm(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "P6"):
            self.run_with(gate, self.screen_hooks("not_ppm"), dut="n305", runs=1)

    def test_screen_channel_rejects_a_frame_index_gap(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "contiguous"):
            self.run_with(gate, self.screen_hooks("gap"), dut="n305", runs=1)

    def test_screen_channel_rejects_a_missing_named_frame(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "missing frame"):
            self.run_with(gate, self.screen_hooks("missing_completion"), dut="n305", runs=1)

    def test_screen_channel_requires_observation_after_the_banner(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "keep observing"):
            self.run_with(gate, self.screen_hooks("completion_last"), dut="n305", runs=1)

    def test_screen_channel_rejects_a_frame_escaping_its_directory(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "bare filename"):
            self.run_with(gate, self.screen_hooks("escape"), dut="n305", runs=1)

    def test_screen_channel_cross_checks_dut_run_and_resolution(self) -> None:
        gate = load_gate()
        for mode, message in (
            ("wrong_dut", "names DUT"),
            ("wrong_run", "expected 1"),
            ("resolution", "verdict declares"),
            ("version", "version is not 1"),
            ("missing_key", "missing required key"),
            ("blank_checker", "name the checker"),
            ("too_few", "at least 2 frames"),
            ("count", "frames but 3 are present"),
            ("malformed", "not key=value"),
            ("duplicate", "repeats key"),
        ):
            with self.subTest(mode=mode):
                with self.assertRaisesRegex(gate.GateError, message):
                    self.run_with(gate, self.screen_hooks(mode), dut="n305", runs=1)

    def test_screen_channel_needs_its_own_hook(self) -> None:
        gate = load_gate()
        hooks = self.screen_hooks("valid")
        del hooks["THEKERNEL_DUT_SCREEN_CAPTURE_CMD"]
        with self.assertRaisesRegex(gate.GateError, "THEKERNEL_DUT_SCREEN_CAPTURE_CMD"):
            self.run_with(gate, hooks, dut="n305", runs=1)

    def test_screen_channel_fails_when_the_hook_writes_nothing(self) -> None:
        gate = load_gate()
        hooks = {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_SCREEN_CAPTURE_CMD": "true",
        }
        with self.assertRaisesRegex(gate.GateError, "no frames"):
            self.run_with(gate, hooks, dut="n305", runs=1)

    def test_a_failing_capture_hook_fails_the_run(self) -> None:
        gate = load_gate()
        hooks = self.screen_hooks("valid")
        hooks["THEKERNEL_DUT_SCREEN_CAPTURE_CMD"] = "exit 7"
        with self.assertRaisesRegex(gate.GateError, "exit status 7"):
            self.run_with(gate, hooks, dut="n305", runs=1)

    def test_network_channel_enforces_the_full_ktap_contract(self) -> None:
        gate = load_gate()
        hook = (
            f"printf '%s\\n' 'KTAP version 1' '1..1' 'ok 1 - complete' "
            f"'{gate.COMPLETION_MARKER}' > \"$THEKERNEL_DUT_TRANSCRIPT\"; "
            f"printf clean > \"$THEKERNEL_DUT_SHUTDOWN_STATUS\""
        )
        hooks = {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_NETWORK_CAPTURE_CMD": hook,
        }
        evidence = self.run_with(gate, hooks, dut="n305", observation="network", runs=1)
        self.assertEqual(evidence[0].channel, "network")

    def test_network_channel_rejects_an_incomplete_transcript(self) -> None:
        gate = load_gate()
        hook = (
            f"printf '%s\\n' 'KTAP version 1' '1..2' 'ok 1 - first' "
            f"> \"$THEKERNEL_DUT_TRANSCRIPT\"; "
            f"printf clean > \"$THEKERNEL_DUT_SHUTDOWN_STATUS\""
        )
        hooks = {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_NETWORK_CAPTURE_CMD": hook,
        }
        with self.assertRaisesRegex(gate.GateError, "plan"):
            self.run_with(gate, hooks, dut="n305", observation="network", runs=1)

    def test_network_channel_requires_an_attested_shutdown(self) -> None:
        gate = load_gate()
        hook = (
            f"printf '%s\\n' 'KTAP version 1' '1..1' 'ok 1 - complete' "
            f"'{gate.COMPLETION_MARKER}' > \"$THEKERNEL_DUT_TRANSCRIPT\""
        )
        hooks = {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_NETWORK_CAPTURE_CMD": hook,
        }
        with self.assertRaisesRegex(gate.GateError, "shutdown status"):
            self.run_with(gate, hooks, dut="n305", observation="network", runs=1)

    def test_each_channel_needs_its_own_hook(self) -> None:
        gate = load_gate()
        serial = {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_SERIAL_CAPTURE_CMD": "true",
        }
        with self.assertRaisesRegex(gate.GateError, "THEKERNEL_DUT_NETWORK_CAPTURE_CMD"):
            self.run_with(gate, serial, dut="n305", observation="network", runs=1)
        with self.assertRaisesRegex(gate.GateError, "THEKERNEL_DUT_SCREEN_CAPTURE_CMD"):
            self.run_with(gate, serial, dut="n305", observation="screen", runs=1)

    def test_the_runner_may_select_the_channel_through_the_environment(self) -> None:
        gate = load_gate()
        hooks = self.screen_hooks("valid")
        hooks["THEKERNEL_DUT_OBSERVATION"] = "screen"
        with mock.patch.dict(os.environ, hooks, clear=False):
            evidence = gate.run_gate(self.artifact_dir, self.state_dir, dut="n305", runs=1)
        self.assertEqual(evidence[0].channel, "screen")

    def test_unknown_dut_and_channel_fail_closed(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "unknown DUT profile"):
            self.run_with(gate, self.screen_hooks("valid"), dut="acme", runs=1)
        with self.assertRaisesRegex(gate.GateError, "unknown observation channel"):
            self.run_with(gate, self.screen_hooks("valid"), dut="n305", observation="telepathy", runs=1)

    def test_only_panther_lake_pins_the_cold_boot_count(self) -> None:
        gate = load_gate()
        with self.assertRaisesRegex(gate.GateError, "exactly three"):
            self.run_with(gate, self.screen_hooks("valid"), dut="panther-lake", runs=2)
        self.run_with(gate, self.screen_hooks("valid"), dut="n305", runs=1)

    def test_serial_channel_still_works_for_existing_runners(self) -> None:
        gate = load_gate()
        serial_hook = (
            "printf '%s\\n' 'KTAP version 1' '1..1' 'ok 1 - complete' "
            "'# THEKERNEL_SYSTEM_TEST_COMPLETE' > \"$THEKERNEL_DUT_SERIAL_LOG\"; "
            "printf clean > \"$THEKERNEL_DUT_SHUTDOWN_STATUS\""
        )
        hooks = {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_SERIAL_CAPTURE_CMD": serial_hook,
        }
        evidence = self.run_with(gate, hooks, runs=3)
        self.assertEqual([item.channel for item in evidence], ["serial"] * 3)
        self.assertTrue((self.state_dir / "cold-boot-3.serial.log").is_file())

    def test_hooks_receive_the_transport_neutral_transcript_name(self) -> None:
        gate = load_gate()
        hook = (
            'test "$THEKERNEL_DUT_TRANSCRIPT" = "$THEKERNEL_DUT_SERIAL_LOG" && '
            'test "$THEKERNEL_DUT_OBSERVATION" = "serial" && '
            f"printf '%s\\n' 'KTAP version 1' '1..1' 'ok 1 - complete' "
            f"'{gate.COMPLETION_MARKER}' > \"$THEKERNEL_DUT_TRANSCRIPT\" && "
            f"printf clean > \"$THEKERNEL_DUT_SHUTDOWN_STATUS\""
        )
        hooks = {
            "THEKERNEL_DUT_POWER_CYCLE_CMD": "true",
            "THEKERNEL_DUT_BOOT_ONCE_CMD": "true",
            "THEKERNEL_DUT_SERIAL_CAPTURE_CMD": hook,
        }
        self.run_with(gate, hooks, runs=3)


class ScreenHelperTests(unittest.TestCase):
    """The capture helper and the gate must agree on the verdict's shape."""

    def setUp(self) -> None:
        self._tmp = test_tmpdir()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.frames = self.root / "frames"
        self.frames.mkdir()
        gate = load_gate()
        for index in range(3):
            body = bytearray(b"\x00\x00\x00" * (320 * 200))
            for pixel in range(0, 320 * 200, 20):  # sparse text on a dark console
                body[pixel * 3 : pixel * 3 + 3] = b"\xff\xff\xff"
            (self.frames / f"frame-{index:04d}.ppm").write_bytes(
                b"P6\n320 200\n255\n" + bytes(body)
            )
        self.helper = Path(repo_root()) / "scripts/ci/n305-screen-capture.sh"
        self.gate = gate

    def run_helper(self, *arguments: str) -> None:
        subprocess.run(
            ("bash", str(self.helper), "verdict", "--dir", str(self.frames), *arguments),
            check=True,
            capture_output=True,
        )

    def test_a_helper_verdict_is_accepted_by_the_gate(self) -> None:
        self.run_helper("--completion-frame", "1", "--checker", "unit test")
        paths = self.gate.RunPaths(
            transcript=self.root / "unused.log",
            shutdown_status=self.root / "unused.status",
            screen_dir=self.frames,
            screen_verdict=self.frames / "verdict.txt",
        )
        self.gate.validate_screen_evidence(paths, "n305", 1)

    def test_a_completion_frame_that_is_the_last_frame_is_refused(self) -> None:
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_helper("--completion-frame", "2", "--checker", "unit test")


class PpmTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = test_tmpdir()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.gate = load_gate()

    def write(self, body: bytes, name: str = "frame-0000.ppm") -> Path:
        path = self.root / name
        path.write_bytes(body)
        return path

    def test_header_comments_and_whitespace_are_accepted(self) -> None:
        path = self.write(b"P6\n# a comment\n2 1\n255\n" + b"\x01\x02\x03" * 2)
        header = self.gate.parse_ppm_header(path)
        self.assertEqual((header.width, header.height, header.maxval), (2, 1, 255))

    def test_a_header_only_file_is_truncated(self) -> None:
        path = self.write(b"P6\n2 1\n255\n")
        with self.assertRaisesRegex(self.gate.GateError, "truncated"):
            self.gate.parse_ppm_header(path)

    def test_sixteen_bit_samples_are_rejected_rather_than_misread(self) -> None:
        path = self.write(b"P6\n2 1\n65535\n" + b"\x00" * 12)
        with self.assertRaisesRegex(self.gate.GateError, "maxval"):
            self.gate.parse_ppm_header(path)

    def test_zero_sized_frames_are_rejected(self) -> None:
        path = self.write(b"P6\n0 1\n255\n")
        with self.assertRaisesRegex(self.gate.GateError, "non-positive"):
            self.gate.parse_ppm_header(path)


if __name__ == "__main__":
    unittest.main()
