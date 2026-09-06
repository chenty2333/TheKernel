"""Focused product-system-test completion and KTAP gate tests."""

from __future__ import annotations

import os
import tempfile
from tests.support import test_tmpdir
import tomllib
import unittest
from unittest.mock import patch
from pathlib import Path

from tests.support import load_script_module, repo_root


REPO_ROOT = repo_root()


def load_product():
    return load_script_module("thekernel_product", "tools/thekernel.py")


class SystemTestGateTests(unittest.TestCase):
    def test_experimental_candidate_cannot_replace_benchmark_baseline(self) -> None:
        product = load_product()
        default = product.Artifacts(Path("/unused"), product.Variant("1G"), "shell")
        candidate = product.Artifacts(Path("/unused"), product.Variant("1G", m5_candidate=True), "shell")
        self.assertNotEqual(default.output_dir, candidate.output_dir)
        self.assertNotEqual(default.cargo_target_dir, candidate.cargo_target_dir)
        self.assertNotIn("sched-wake-locality", product.kernel_features(default))
        self.assertIn("sched-wake-locality", product.kernel_features(candidate))
        self.assertIn("io-submit-batch", product.kernel_features(candidate))
        args = product.build_parser().parse_args(["bench", "--suite", "all", "--m5-candidate"])
        with self.assertRaisesRegex(product.ProductError, "baseline must use default"):
            product.bench_cmd(args)

    def test_benchmark_no_build_does_not_create_a_missing_linux_esp(self) -> None:
        product = load_product()
        with test_tmpdir() as directory, patch.dict(os.environ, {"THEKERNEL_STATE_DIR": directory}):
            kernel = Path(directory) / "linux"
            kernel.write_bytes(b"Linux")
            args = product.build_parser().parse_args([
                "bench", "--suite", "io", "--no-build", "--linux-kernel", str(kernel)])
            with patch.object(product, "run_checked") as build:
                with self.assertRaisesRegex(product.ProductError, "existing Linux ESP"):
                    product.bench_cmd(args)
            build.assert_not_called()

    def test_rootfs_cache_distinguishes_same_name_in_different_directories(self) -> None:
        from tools import product_state

        with test_tmpdir() as directory:
            root = Path(directory)
            (root / "first").mkdir()
            (root / "second").mkdir()
            source = root / "first" / "probe.c"
            source.write_text("same source")
            with patch.object(product_state, "REPO_ROOT", root), \
                    patch.object(product_state, "ROOTFS_INPUT_FILES", ()), \
                    patch.object(product_state, "ROOTFS_INPUT_GLOBS", ("*/*.c",)):
                before = product_state.rootfs_fingerprint()
                source.rename(root / "second" / "probe.c")
                self.assertNotEqual(before, product_state.rootfs_fingerprint())

    def test_clean_rejects_an_active_operation(self) -> None:
        product = load_product()
        with test_tmpdir() as directory, patch.dict(os.environ, {"THEKERNEL_STATE_DIR": directory}):
            output = Path(directory) / "out"
            output.mkdir()
            with product.state_lock("activity", shared=True):
                self.assertEqual(product.main(["clean"]), 2)
            self.assertTrue(output.is_dir())
            self.assertEqual(product.main(["clean"]), 0)
            self.assertFalse(output.exists())

    def test_configuration_stamp_rejects_changed_rootfs(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            artifacts.output_dir.mkdir(parents=True)
            artifacts.rootfs.parent.mkdir(parents=True)
            artifacts.rootfs.write_bytes(b"initial")
            artifacts.kernel.write_bytes(b"kernel")
            artifacts.esp.write_bytes(b"esp")
            product.rootfs_stamp_path(artifacts).write_text(product.rootfs_fingerprint())
            stamp = product.artifact_config_stamp(artifacts, "module")
            stamp.write_text(product.artifact_config_key(artifacts, None, "module"))
            product.validate_artifact_config(artifacts, None, "module")
            before = artifacts.rootfs.stat()
            artifacts.rootfs.write_bytes(b"changed")
            os.utime(artifacts.rootfs, ns=(before.st_atime_ns, before.st_mtime_ns))
            with self.assertRaises(product.ProductError):
                product.validate_artifact_config(artifacts, None, "module")

    def test_configuration_stamp_rejects_mixed_or_changed_boot_artifacts(self) -> None:
        product = load_product()
        for changed in ("kernel", "esp"):
            with self.subTest(changed=changed), test_tmpdir() as directory:
                artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
                artifacts.output_dir.mkdir(parents=True)
                artifacts.rootfs.parent.mkdir(parents=True)
                artifacts.rootfs.write_bytes(b"rootfs")
                artifacts.kernel.write_bytes(b"kernel")
                artifacts.esp.write_bytes(b"esp")
                product.rootfs_stamp_path(artifacts).write_text(product.rootfs_fingerprint())
                product.artifact_config_stamp(artifacts, "module").write_text(
                    product.artifact_config_key(artifacts, None, "module"))
                getattr(artifacts, changed).write_bytes(b"different")
                with self.assertRaises(product.ProductError):
                    product.validate_artifact_config(artifacts, None, "module")

    def test_no_build_rejects_old_guest_programs(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            artifacts.rootfs.parent.mkdir(parents=True)
            artifacts.rootfs.write_bytes(b"rootfs")
            product.rootfs_stamp_path(artifacts).write_text("old test inputs")
            with self.assertRaisesRegex(product.ProductError, "guest test sources"):
                product.validate_artifact_config(artifacts, None, "module")

    def test_build_inputs_normalize_effective_environment(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            artifacts.rootfs.parent.mkdir(parents=True)
            artifacts.rootfs.write_bytes(b"rootfs")
            with patch.dict(os.environ, {"AX_LOG": "", "AX_BACKTRACE": "", "RUSTFLAGS": "  "}):
                before = product.artifact_input_key(artifacts, None, "module")
            with patch.dict(os.environ, {"AX_LOG": "info", "AX_BACKTRACE": "n", "RUSTFLAGS": ""}):
                self.assertEqual(before, product.artifact_input_key(artifacts, None, "module"))
            with patch.dict(os.environ, {"AX_LOG": "debug", "AX_BACKTRACE": "n", "RUSTFLAGS": ""}):
                self.assertNotEqual(before, product.artifact_input_key(artifacts, None, "module"))

    def test_rootfs_cache_tracks_explicit_uapi_header_locations(self) -> None:
        product = load_product()
        for name in ("THEKERNEL_MUSL_LINUX_UAPI_INCLUDE", "THEKERNEL_MUSL_LINUX_ARCH_INCLUDE"):
            with self.subTest(name=name), patch.dict(os.environ, {name: "/first"}):
                before = product.rootfs_fingerprint()
                os.environ[name] = "/second"
                self.assertNotEqual(before, product.rootfs_fingerprint())

    def test_encoded_flags_cannot_bypass_product_linker_flags(self) -> None:
        product = load_product()
        artifacts = product.Artifacts(Path("/unused"), product.Variant("1G"))
        with patch.dict(os.environ, {"CARGO_ENCODED_RUSTFLAGS": "-Cstrip=none", "RUSTFLAGS": "--cfg custom"}):
            env = product.command_env(artifacts)
        self.assertNotIn("CARGO_ENCODED_RUSTFLAGS", env)
        self.assertIn("--cfg custom", env["RUSTFLAGS"])
        self.assertIn(str(artifacts.linker_script), env["RUSTFLAGS"])

    def test_tmpfs_state_is_rejected(self) -> None:
        product = load_product()
        with patch.dict(os.environ, {"THEKERNEL_STATE_DIR": "/dev/shm/thekernel-test"}):
            with self.assertRaises(product.ProductError):
                product.state_root()

    def test_failed_objcopy_preserves_published_kernel(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            artifacts.output_dir.mkdir(parents=True)
            artifacts.rootfs.parent.mkdir(parents=True)
            artifacts.rootfs.write_bytes(b"rootfs")
            artifacts.kernel.write_bytes(b"known working kernel")
            artifacts.cargo_elf.parent.mkdir(parents=True)
            artifacts.cargo_elf.write_bytes(b"new ELF")

            def execute(argv, **_kwargs):
                if argv[0] == "/mock/objcopy":
                    Path(argv[-1]).write_bytes(b"partial")
                    raise product.ProductError("objcopy failed")

            with patch.object(product, "generate_config"), \
                    patch.object(product, "llvm_objcopy", return_value=Path("/mock/objcopy")), \
                    patch.object(product, "run_checked", side_effect=execute):
                with self.assertRaisesRegex(product.ProductError, "objcopy failed"):
                    product.build_kernel(artifacts)
            self.assertEqual(artifacts.kernel.read_bytes(), b"known working kernel")
            self.assertFalse(product.artifact_config_stamp(artifacts, "module").exists())

    def test_rootfs_inputs_changing_during_build_do_not_publish_success_stamp(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            with patch.object(product, "rootfs_fingerprint", side_effect=["before", "after"]), \
                    patch.object(product, "run_checked"):
                with self.assertRaisesRegex(product.ProductError, "changed during compilation"):
                    product.build_rootfs(artifacts)
            self.assertFalse(product.rootfs_stamp_path(artifacts).exists())

    def test_kernel_inputs_changing_during_build_do_not_publish_success_stamp(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            artifacts.rootfs.parent.mkdir(parents=True)
            artifacts.rootfs.write_bytes(b"rootfs")
            artifacts.cargo_elf.parent.mkdir(parents=True)
            artifacts.cargo_elf.write_bytes(b"ELF")

            def execute(argv, **_kwargs):
                if argv[0] == "/mock/objcopy":
                    Path(argv[-1]).write_bytes(b"complete kernel")

            with patch.object(product, "artifact_input_key", side_effect=["before", "after"]), \
                    patch.object(product, "generate_config"), \
                    patch.object(product, "llvm_objcopy", return_value=Path("/mock/objcopy")), \
                    patch.object(product, "run_checked", side_effect=execute):
                with self.assertRaisesRegex(product.ProductError, "changed during build"):
                    product.build_kernel(artifacts)
            self.assertFalse(product.artifact_config_stamp(artifacts, "module").exists())

    def test_host_suite_discovers_reusable_components_separately(self) -> None:
        product = load_product()
        metadata = {"packages": [
            {"name": "mechanism-example", "metadata": {"thekernel": {"layer": "mechanism"}}},
            {"name": "linux-example", "metadata": {"thekernel": {"layer": "linux_abi"}}},
            {"name": "platform-example", "metadata": {"thekernel": {"layer": "platform"}}},
            {"name": "thekernel-axtask", "metadata": {"thekernel": {"layer": "platform",
                "host-test": {"selected": True, "features": ["test", "sched-eevdf"], "all-targets": True}}}},
        ]}
        from types import SimpleNamespace
        with test_tmpdir() as directory, patch.dict(os.environ, {"THEKERNEL_STATE_DIR": directory}), \
                patch.object(product.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout=product.json.dumps(metadata))), \
                patch.object(product, "run_checked") as commands:
            self.assertEqual(product.host_test_cmd(), 0)
        invocations = [call.args[0] for call in commands.call_args_list]
        for package in ("mechanism-example", "linux-example"):
            self.assertIn(["cargo", "test", "--locked", "-p", package, "--target", "x86_64-unknown-linux-gnu"], invocations)
        self.assertFalse(any("platform-example" in command for command in invocations))
        self.assertTrue(any("thekernel-axtask" in command and "--features" in command for command in invocations))

    def test_host_suite_isolates_product_build_environment(self) -> None:
        product = load_product()
        from types import SimpleNamespace
        inherited = {"RUSTFLAGS": "-C link-arg=-Tkernel.lds",
                     "CARGO_ENCODED_RUSTFLAGS": "--cfg\x1fproduct",
                     "CARGO_BUILD_TARGET": "x86_64-unknown-none",
                     "RUST_TEST_THREADS": "8", "TMPDIR": "/dev/shm"}
        with test_tmpdir() as directory, \
                patch.dict(os.environ, {**inherited, "THEKERNEL_STATE_DIR": directory}), \
                patch.object(product.subprocess, "run", return_value=SimpleNamespace(
                    returncode=0, stdout=product.json.dumps({"packages": []}))), \
                patch.object(product, "run_checked") as commands:
            self.assertEqual(product.host_test_cmd(), 0)
            env = commands.call_args_list[-1].kwargs["env"]
            for variable in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"):
                self.assertNotIn(variable, env)
            self.assertEqual(env["RUST_TEST_THREADS"], "1")
            self.assertEqual(env["TMPDIR"], str(Path(directory) / "test-tmp"))
            self.assertIn("percpu.x", env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"])

    def test_component_host_test_features_are_explicit(self) -> None:
        product = load_product()
        package = {"name": "allocator", "metadata": {"thekernel": {"host-test": {
            "features": ["full"], "default-features": False,
            "target": "x86_64-unknown-linux-gnu"}}}}
        self.assertEqual(product.component_host_test_command(package), [
            "cargo", "test", "--locked", "-p", "allocator", "--features", "full",
            "--no-default-features", "--target", "x86_64-unknown-linux-gnu"])

    def test_product_state_defaults_to_the_host_cache(self) -> None:
        product = load_product()
        previous = os.environ.pop("THEKERNEL_STATE_DIR", None)
        try:
            self.assertEqual(
                product.state_root(), Path.home() / ".cache" / "thekernel-targets"
            )
        finally:
            if previous is not None:
                os.environ["THEKERNEL_STATE_DIR"] = previous

    def test_product_feature_aggregation_is_the_standard_build_baseline(self) -> None:
        product = load_product()
        root_manifest = tomllib.loads((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))

        self.assertEqual(
            root_manifest["features"][product.PRODUCT_FEATURE],
        ["qemu", "smp", "hwp-uclamp", "pmu", "perf-sampling"],
        )
        args = product.build_parser().parse_args(["build"])
        self.assertEqual(
            product.kernel_features(product.Artifacts(Path("state"), product.parse_variant(args))),
            "x86-product",
        )

    def test_product_feature_combines_variant_features_without_repeating_baseline(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(
            ["build", "--smp", "1", "--asid-fast-switch", "--profile", "shell"]
        )

        self.assertEqual(
            product.kernel_features(product.Artifacts(Path("state"), product.parse_variant(args), args.profile)),
            "x86-product boot-shell asid-fast-switch",
        )

    def test_product_defaults_and_compile_time_network_match_q35_gate(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["test", "--suite", "guest"])
        self.assertEqual((args.smp, args.memory), (4, "1G"))
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(
                Path(directory), product.parse_variant(args), "system"
            )
            environment = product.command_env(artifacts)
        self.assertEqual(environment["AX_IP"], "10.0.2.15")
        self.assertEqual(environment["AX_GW"], "10.0.2.2")
        self.assertEqual(environment["SMOLTCP_IFACE_MAX_ADDR_COUNT"], "4")
        self.assertIn("--cfg aes_force_soft", environment["RUSTFLAGS"])
        platform = tomllib.loads(
            (REPO_ROOT / "config/x86_64/q35-uefi.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(platform["devices"]["pci-ecam-base"], 0xE000_0000)
        self.assertIn(
            [0xE000_0000, 0x1000_0000], platform["devices"]["mmio-ranges"]
        )

    def test_q35_accepts_high_ram_without_expanding_the_low_ram_window(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["build", "--memory", "4G"])
        variant = product.parse_variant(args)
        self.assertEqual(variant.memory_bytes, 4 * product.GIB)
        self.assertEqual(product.Q35_PCI_HOLE_LOW_RAM_LIMIT, 2 * product.GIB)
        self.assertEqual(product.Q35_HIGH_MEMORY_BASE, 4 * product.GIB)

    def test_ktap_skip_is_rejected_by_default_gate(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            log = Path(directory) / "console.log"
            log.write_text(
                "KTAP version 1\nok 1 - supported\nok 2 - unavailable # SKIP guest ABI\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(product.ProductError, "KTAP SKIP"):
                product.reject_ktap_skips_in_log(log)

    def test_ktap_without_skip_remains_acceptable(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            log = Path(directory) / "console.log"
            log.write_text("KTAP version 1\nok 1 - supported\n", encoding="utf-8")
            product.reject_ktap_skips_in_log(log)

    def test_graphics_smoke_flavors_come_from_the_dual_parsed_manifest(self) -> None:
        product = load_product()
        flavors = product.graphics_flavors()
        self.assertEqual(flavors, (
            "headless-abi-smoke",
            "q35-graphics-seatd",
            "q35-software-desktop",
            "q35-graphics-benchmark",
            "q35-venus-desktop",
            "q35-graphics-logind",
        ))
        self.assertEqual(product.graphics_smoke_flavors(), (
            "headless-abi-smoke",
            "q35-graphics-seatd",
            "q35-graphics-logind",
        ))
        args = product.build_parser().parse_args([
            "test", "--suite", "graphics", "--no-build", "--rootfs", "rootfs.ext2",
            "--screenshot", "graphics.ppm", "--flavor", "q35-graphics-seatd",
        ])
        self.assertEqual(args.flavor, "q35-graphics-seatd")
        with self.assertRaises(SystemExit):
            product.build_parser().parse_args([
                "test", "--suite", "graphics", "--no-build", "--rootfs", "rootfs.ext2",
                "--screenshot", "graphics.ppm", "--flavor", "q35-graphics-benchmark",
            ])

    def test_system_test_configures_marker_gated_shutdown_not_runner_stop(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(["test", "--suite", "guest"])
        calls: dict[str, object] = {}

        def fake_build(_artifacts):
            return None

        def fake_run_product(_artifacts, spec):
            calls["spec"] = spec
            return 0

        original_build_kernel = product.build_kernel
        original_build_rootfs = product.build_rootfs
        original_run_product = product.run_product
        try:
            product.build_kernel = fake_build
            product.build_rootfs = fake_build
            product.run_product = fake_run_product
            self.assertEqual(product.system_test_cmd(args), 0)
        finally:
            product.build_kernel = original_build_kernel
            product.build_rootfs = original_build_rootfs
            product.run_product = original_run_product

        spec = calls["spec"]
        self.assertTrue(spec.shutdown_after_marker)
        self.assertTrue(spec.reject_ktap_skips)
        self.assertEqual(spec.rootfs_transport, "module")
        self.assertIsNone(spec.stop_after_marker)

    def test_system_test_run_cpus_selects_the_qemu_cpu_count(self) -> None:
        product = load_product()
        args = product.build_parser().parse_args(
            ["test", "--suite", "guest", "--smp", "4", "--run-cpus", "1", "--no-build"]
        )
        observed: dict[str, object] = {}

        def fake_run_product(artifacts, spec):
            observed["variant_name"] = artifacts.variant.name
            observed["run_cpus"] = spec.run_cpus
            return 0

        original_run_product = product.run_product
        try:
            product.run_product = fake_run_product
            self.assertEqual(product.system_test_cmd(args), 0)
        finally:
            product.run_product = original_run_product

        self.assertEqual(observed["variant_name"], "mem1g")
        self.assertEqual(observed["run_cpus"], 1)

    def test_qemu_debug_cli_is_optional_for_run_and_test(self) -> None:
        product = load_product()
        for command in (["run"], ["test", "--suite", "guest"]):
            with self.subTest(command=command):
                self.assertIsNone(product.build_parser().parse_args(command).qemu_debug)
                args = product.build_parser().parse_args(command + ["--qemu-debug", "guest_errors,cpu_reset,int"])
                self.assertEqual(args.qemu_debug, "guest_errors,cpu_reset,int")

    def test_run_product_uses_run_cpu_override_for_qemu_command(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(
                root / "state", product.Variant(memory="1G"), "system"
            )
            for path in (artifacts.kernel, artifacts.esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            observed = {}

            def fake_run(config):
                observed["cpus"] = config.cpus
                observed["extra_args"] = config.extra_args
                return type("Result", (), {
                    "returncode": 0,
                    "error_message": None,
                    "log_path": config.log_path,
                    "diagnostic_log_path": config.workdir / "kernel.log",
                    "guest_clean_shutdown": True,
                    "intentionally_stopped": False,
                })()

            original_run = product.run
            try:
                product.run = fake_run
                self.assertEqual(product.run_product.__wrapped__(
                    artifacts,
                    product.RunSpec(
                        accel="tcg",
                        timeout=30,
                        workdir=root / "run",
                        interactive=False,
                        input_after_marker=None,
                        stop_after_marker=None,
                        commands=None,
                        extra_block=None,
                        run_cpus=1,
                        qemu_debug="guest_errors,cpu_reset,int",
                    ),
                ), 0)
            finally:
                product.run = original_run

        self.assertEqual(observed["cpus"], 1)
        self.assertEqual(observed["extra_args"], (
            "-d", "guest_errors,cpu_reset,int", "-D", str(root / "run" / "qemu-debug.log")))

    def test_run_product_drive_uses_the_separate_drive_esp(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(
                root / "state", product.Variant(memory="1G"), "system"
            )
            for path in (artifacts.kernel, artifacts.drive_esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            observed = {}

            def fake_run(config):
                observed["config"] = config
                return type("Result", (), {
                    "returncode": 0,
                    "error_message": None,
                    "log_path": config.log_path,
                    "diagnostic_log_path": config.workdir / "kernel.log",
                    "guest_clean_shutdown": True,
                    "intentionally_stopped": False,
                })()

            original_run = product.run
            try:
                product.run = fake_run
                self.assertEqual(product.run_product.__wrapped__(
                    artifacts,
                    product.RunSpec(
                        accel="tcg",
                        timeout=30,
                        workdir=root / "run",
                        interactive=False,
                        input_after_marker=None,
                        stop_after_marker=None,
                        commands=None,
                        extra_block=None,
                        rootfs_transport="drive",
                        run_cpus=4,
                    ),
                ), 0)
            finally:
                product.run = original_run

        self.assertEqual(observed["config"].esp, artifacts.drive_esp)
        self.assertEqual(observed["config"].rootfs_transport, "drive")

    def test_run_cpus_rejects_values_outside_the_smp_bound(self) -> None:
        product = load_product()
        with self.assertRaisesRegex(product.ProductError, "--run-cpus"):
            product.resolve_run_cpus(4, 0)
        with self.assertRaisesRegex(product.ProductError, "--run-cpus"):
            product.resolve_run_cpus(4, 5)

    def test_explicit_new_workdir_exists_before_shutdown_commands_are_written(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(
                root / "state", product.Variant(memory="1G"), "system"
            )
            for path in (artifacts.kernel, artifacts.esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            workdir = root / "new" / "system-test"
            observed = {}

            def fake_run(config):
                observed["config"] = config
                self.assertTrue(config.workdir.is_dir())
                self.assertEqual(
                    config.input_path.read_text(encoding="utf-8"),
                    product.SYSTEM_TEST_SHUTDOWN_COMMANDS,
                )
                return type("Result", (), {
                    "returncode": 0,
                    "error_message": None,
                    "log_path": config.log_path,
                    "diagnostic_log_path": config.workdir / "kernel.log",
                    "guest_clean_shutdown": True,
                    "intentionally_stopped": False,
                })()

            original_run = product.run
            try:
                product.run = fake_run
                self.assertEqual(product.run_product.__wrapped__(
                    artifacts,
                    product.RunSpec(
                        accel="tcg",
                        timeout=30,
                        workdir=workdir,
                        interactive=False,
                        input_after_marker=None,
                        stop_after_marker=None,
                        commands=None,
                        extra_block=None,
                        shutdown_after_marker=True,
                        run_cpus=4,
                    ),
                ), 0)
            finally:
                product.run = original_run

            self.assertEqual(observed["config"].workdir, workdir.resolve())


if __name__ == "__main__":
    unittest.main()
