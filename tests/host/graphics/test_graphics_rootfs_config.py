#!/usr/bin/env python3
"""Static contract tests for the non-logind graphics userspace session."""
from __future__ import annotations

import pathlib
import os
import subprocess
import shlex
import socket
import tempfile
from tests.support import test_tmpdir
from types import SimpleNamespace
import unittest
from unittest import mock

from tests.support import load_script_module, repo_root


ROOT = repo_root()
GRAPHICS = ROOT / "config" / "graphics"


class GraphicsRootfsConfigTests(unittest.TestCase):
    def test_failed_repeated_graphics_benchmark_removes_previous_metrics(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        with test_tmpdir() as directory:
            root = pathlib.Path(directory)
            rootfs, oracle = root / "rootfs.ext2", root / "linux.log"
            rootfs.write_bytes(b"rootfs")
            oracle.write_text("oracle")
            result = root / "graphics-metrics.json"
            result.write_text('{"previous":"success"}')
            args = SimpleNamespace(accel="kvm", smp=4, memory="4G", asid_fast_switch=False,
                rootfs=str(rootfs), workdir=str(root), linux_oracle_log=str(oracle),
                no_build=True, timeout=10, graphics_profile="headless", fault=None)
            with mock.patch.object(module, "run_product", return_value=4):
                self.assertEqual(module.graphics_benchmark_cmd(args), 4)
            self.assertFalse(result.exists())
            self.assertEqual(oracle.read_text(), "oracle")

    def test_graphics_metrics_alias_is_rejected_before_cleanup(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        with test_tmpdir() as directory:
            root = pathlib.Path(directory)
            rootfs, oracle = root / "rootfs.ext2", root / "linux.log"
            rootfs.write_bytes(b"rootfs")
            oracle.write_text("oracle")
            result = root / "graphics-metrics.json"
            result.hardlink_to(oracle)
            args = SimpleNamespace(accel="kvm", smp=4, memory="4G", asid_fast_switch=False,
                rootfs=str(rootfs), workdir=str(root), linux_oracle_log=str(oracle),
                no_build=True, timeout=10, graphics_profile="headless", fault=None)
            with mock.patch.object(module, "run_product") as run:
                with self.assertRaisesRegex(module.ProductError, "aliases"):
                    module.graphics_benchmark_cmd(args)
                run.assert_not_called()
            self.assertEqual(result.read_text(), "oracle")
            self.assertEqual(oracle.read_text(), "oracle")

    def read(self, relative: str) -> str:
        return (GRAPHICS / relative).read_text()

    def test_common_config_builds_an_ext2_image_against_linux_6_12_headers(self) -> None:
        common = self.read("common.config")
        self.assertIn("BR2_TARGET_ROOTFS_EXT2=y", common)
        self.assertNotIn("BR2_ROOTFS_EXT2=y", common)
        # The guest UAPI oracles must compile against the same 6.12 headers as
        # the standalone Linux oracle kernel.
        self.assertIn("BR2_KERNEL_HEADERS_6_12=y", common)
        self.assertNotIn("SYSTEMD_LOGIND=y", common)

    def test_flavor_manifest_matches_the_checked_in_fragments_and_overlays(self) -> None:
        manifest: dict[str, str] = {}
        for line in self.read("flavors.env").splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, separator, value = line.partition("=")
            self.assertEqual(separator, "=", line)
            manifest[key] = value.strip().strip('"')
        flavors = manifest["FLAVORS"].split()
        self.assertEqual(flavors, [
            "headless-abi-smoke",
            "q35-graphics-seatd",
            "q35-software-desktop",
            "q35-graphics-benchmark",
            "q35-venus-desktop",
            "q35-graphics-logind",
        ])
        for flavor in flavors:
            self.assertTrue((GRAPHICS / f"{flavor}.fragment").is_file(), flavor)
            key = flavor.upper().replace("-", "_")
            overlay = manifest[f"FLAVOR_{key}_OVERLAY"]
            self.assertTrue((GRAPHICS / "overlay" / overlay).is_dir(), flavor)
            self.assertIn(manifest[f"FLAVOR_{key}_SESSION"], ("seatd", "logind"))
            backend = manifest[f"FLAVOR_{key}_BACKEND"]
            self.assertIn(
                backend,
                ("headless-backend.so", "drm-backend.so", "none"),
            )
            if backend != "none":
                link = GRAPHICS / "overlay" / overlay / "etc" / "weston" / "weston.ini"
                self.assertTrue(link.is_symlink(), flavor)
                expected = "weston-headless.ini" if backend == "headless-backend.so" else "weston-drm.ini"
                if flavor == "q35-software-desktop":
                    expected = "weston-desktop.ini"
                self.assertEqual(link.readlink(), pathlib.Path(expected), flavor)
        for flavor in manifest["SMOKE_FLAVORS"].split():
            self.assertIn(flavor, flavors)
        for flavor in manifest["CI_CHECK_FLAVORS"].split():
            self.assertIn(flavor, flavors)

    def test_seatd_init_waits_for_the_socket_before_weston_starts(self) -> None:
        seatd = self.read("overlay/common/etc/init.d/S70seatd")
        self.assertIn("while [ ! -S /run/seatd.sock ]", seatd)

    def test_weston_prepares_shared_x11_socket_directory_before_dropping_privileges(self) -> None:
        weston = self.read("overlay/common/etc/init.d/S80weston")
        prepare = (
            "    mkdir -p /tmp/.X11-unix\n"
            "    chown root:root /tmp/.X11-unix\n"
            "    chmod 1777 /tmp/.X11-unix\n"
        )
        self.assertIn(prepare, weston)
        self.assertLess(weston.index(prepare), weston.index('start-stop-daemon -S'))

    def test_weston_readiness_requires_live_process_and_socket_with_bounded_wait(self) -> None:
        weston = self.read("overlay/common/etc/init.d/S80weston")
        functions = weston[weston.index("running() {"):weston.index('case "${1:-}" in')]
        with test_tmpdir() as directory, socket.socket(socket.AF_UNIX) as listener:
            root = pathlib.Path(directory)
            listener.bind(str(root / "ready"))
            for kind, alive, expected, waits in (
                ("socket", True, 0, 0),
                ("socket", False, 1, 0),
                ("file", True, 1, 20),
                ("absent", True, 1, 20),
                ("delayed", True, 0, 2),
            ):
                with self.subTest(kind=kind, alive=alive):
                    path = root / "wayland-0"
                    path.unlink(missing_ok=True)
                    if kind == "socket":
                        path.symlink_to(root / "ready")
                    elif kind == "file":
                        path.touch()
                    setup = f"RUNTIME_DIR={shlex.quote(str(root))}\n" + functions
                    setup += "pid_from_file() { echo 123; }\n"
                    setup += f"running() {{ {'true' if alive else 'false'}; }}\n"
                    setup += "sleep() { echo wait; "
                    if kind == "delayed":
                        setup += '[ "$tries" -ne 2 ] || ln -s "$RUNTIME_DIR/ready" "$RUNTIME_DIR/wayland-0"; '
                    setup += "}\nwait_for_ready\n"
                    result = subprocess.run(["sh"], input=setup, text=True, capture_output=True)
                    self.assertEqual(result.returncode, expected, result.stderr)
                    self.assertEqual(result.stdout.splitlines().count("wait"), waits)

    def test_graphics_session_reports_failure_before_weston_exec(self) -> None:
        source = self.read("overlay/common/usr/local/bin/graphics-session")
        with test_tmpdir() as directory:
            root = pathlib.Path(directory)
            runtime, home = root / "runtime", root / "home"
            runtime.mkdir(mode=0o700)
            home.mkdir()
            (home / ".config").write_text("not a directory")
            flavor = root / "flavor"
            flavor.write_text("q35-software-desktop\n")
            source = source.replace("runtime_dir=/run/user/$(id -u)", f"runtime_dir={shlex.quote(str(runtime))}")
            source = source.replace("export HOME=/var/lib/weston", f"export HOME={shlex.quote(str(home))}")
            source = source.replace("/etc/thekernel-graphics-flavor", shlex.quote(str(flavor)))
            # Host CI can run as root; retain the runtime owner/mode validation.
            source = source.replace('[ "$(id -u)" -ne 0 ]', "true")
            result = subprocess.run(["sh"], input=source, text=True, capture_output=True,
                env={key: value for key, value in os.environ.items() if key != "XDG_CONFIG_HOME"})
            self.assertNotEqual(result.returncode, 0)
            log = (runtime / "graphics-session.log").read_text()
            self.assertIn("mkdir:", log)
            self.assertIn("graphics-session failed during desktop user configuration", log)

    def test_software_sessions_select_pixman_and_seatd_verifies_it(self) -> None:
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertIn(
            "if grep -Eqx 'q35-(graphics-seatd|software-desktop)' /etc/thekernel-graphics-flavor &&\n"
            "    [ ! -c /dev/dri/renderD128 ]; then\n"
            '    set -- "$@" --renderer=pixman\n'
            'elif [ -c /dev/dri/renderD128 ]; then\n'
            '    set -- "$@" --renderer=gl\nfi', session,
        )
        self.assertIn(
            'set -- --config=/etc/weston/weston.ini --socket=wayland-0 '
            '--log="$XDG_RUNTIME_DIR/weston.log"', session,
        )
        self.assertIn('exec weston "$@"', session)
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        renderer_check = smoke.index("grep -Fq 'Using Pixman renderer'")
        self.assertLess(renderer_check, smoke.index("export THEKERNEL_GRAPHICS_SMOKE_EXIT=1"))
        self.assertIn('state=FAIL reason=pixman_renderer', smoke)

    def test_accelerated_xwayland_removes_inherited_glamor_disable(self) -> None:
        # Xwayland checks presence, so assigning "0" still disables glamor.
        # Execute the wrappers' environment setup with a known character node.
        for wrapper in (
            "overlay/common/usr/local/bin/thekernel-xwayland-glamor",
            "overlay/q35-software-desktop/usr/local/bin/thekernel-desktop-xwayland",
        ):
            with self.subTest(wrapper=wrapper):
                setup = self.read(wrapper).split("exec /usr/bin/Xwayland", 1)[0]
                setup = setup.replace("/dev/dri/renderD128", "/dev/null")
                result = subprocess.run(
                    ["sh"], input=setup + 'printf "%s" "${XWAYLAND_NO_GLAMOR-unset}"\n',
                    env={**os.environ, "XWAYLAND_NO_GLAMOR": "1"},
                    text=True, capture_output=True, check=True,
                )
                self.assertEqual(result.stdout, "unset")

    def test_desktop_file_launches_preserve_relative_paths_and_browser_arguments(self) -> None:
        with test_tmpdir() as directory:
            root = pathlib.Path(directory)
            home, documents = root / "home", root / "documents"
            home.mkdir()
            documents.mkdir()
            recorder = root / "record"
            recorder.write_text('#!/bin/sh\nprintf "%s\\n" "$PWD" "$@"\n')
            recorder.chmod(0o755)
            wrapper = self.read("overlay/q35-software-desktop/usr/local/bin/thekernel-desktop-app")
            wrapper = wrapper.replace("export HOME=/var/lib/weston", f"export HOME={shlex.quote(str(home))}")
            wrapper = wrapper.replace("/usr/bin/xedit", shlex.quote(str(recorder)))
            wrapper = wrapper.replace("/usr/bin/dbus-run-session", shlex.quote(str(recorder)))
            cookie_file = home / ".local/share/webkitgtk-4.1/MiniBrowser/cookies.sqlite"
            browser_prefix = ["--", "/usr/bin/MiniBrowser", "--enable-sandbox", f"--cookies-file={cookie_file}"]
            for app, arguments, expected in (
                ("editor", ["relative file.py"], ["relative file.py"]),
                ("browser", ["https://example.test/?a=1&b=2"],
                    [*browser_prefix, "https://example.test/?a=1&b=2"]),
                ("browser", [], [*browser_prefix, "about:blank"]),
            ):
                with self.subTest(app=app, arguments=arguments):
                    result = subprocess.run(["sh", "-s", "--", app, *arguments], input=wrapper,
                        cwd=documents, env={key: value for key, value in os.environ.items() if key != "XDG_DATA_HOME"},
                        text=True, capture_output=True, check=True)
                    self.assertEqual(result.stdout.splitlines(), [str(documents), *expected])
                    if app == "browser":
                        self.assertTrue(cookie_file.parent.is_dir())

    def test_desktop_stop_waits_for_audio_even_when_weston_has_exited(self) -> None:
        script = self.read("overlay/common/etc/init.d/S80weston")
        functions = script[script.index("running() {"):script.index('case "${1:-}" in')]
        for stuck in (False, True):
            with self.subTest(stuck=stuck):
                harness = '''
set -eu
WESTON_HOME_MOUNT=/persistent-home
USER=weston
PIDFILE=/unused
remaining=2
start-stop-daemon() {
    case " $* " in
        *" -t "*)
            [ "$remaining" -gt 0 ] || return 1
            [ "$stuck" = yes ] || remaining=$((remaining - 1))
            return 0 ;;
        *) echo audio-stop ;;
    esac
}
sleep() { echo wait; }
'''
                harness += f"stuck={'yes' if stuck else 'no'}\n" + functions
                harness += "\npid_from_file() { return 1; }\nrm() { :; }\nstop\n"
                result = subprocess.run(["sh"], input=harness, text=True, capture_output=True)
                self.assertEqual(result.returncode, 1 if stuck else 0)
                self.assertEqual(result.stdout.splitlines().count("wait"), 10 if stuck else 2)
                self.assertIn("audio-stop", result.stdout)
                if stuck:
                    self.assertIn("PulseAudio did not stop", result.stderr)

    def test_desktop_seeds_standard_user_directories_without_replacing_user_choices(self) -> None:
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        setup = session[session.index("    config_dir="):session.index("    pulseaudio --start")]
        template = GRAPHICS / "overlay/q35-software-desktop/etc/xdg/user-dirs.dirs"
        setup = setup.replace("/etc/xdg/user-dirs.dirs", shlex.quote(str(template)))
        with test_tmpdir() as directory:
            config = pathlib.Path(directory) / "config"
            env = {**os.environ, "XDG_CONFIG_HOME": str(config)}
            subprocess.run(["sh", "-eu"], input=setup, env=env, text=True, check=True)
            user_dirs = config / "user-dirs.dirs"
            self.assertEqual(user_dirs.read_text(), template.read_text())
            user_dirs.write_text('XDG_DOWNLOAD_DIR="$HOME/My Downloads"\n')
            subprocess.run(["sh", "-eu"], input=setup, env=env, text=True, check=True)
            self.assertEqual(user_dirs.read_text(), 'XDG_DOWNLOAD_DIR="$HOME/My Downloads"\n')

    def test_weston_smoke_verifies_initialized_drm_backend_and_device(self) -> None:
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertNotIn('/environ', smoke)
        self.assertNotIn("grep -q 'drm-backend.so'", smoke)
        self.assertIn('report_weston_failure pixman_client_exit', smoke)
        self.assertIn("Seat opened with backend 'seatd'", smoke)
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertIn("export LIBSEAT_BACKEND=seatd", session)
        backend = smoke.index("grep -Fq 'initializing drm backend'")
        self.assertLess(smoke.index("weston_log=$XDG_RUNTIME_DIR/weston.log"), backend)
        self.assertIn(
            "grep -Fq 'initializing drm backend' \"$weston_log\" &&\n"
            "    grep -Fq 'using /dev/dri/card0' \"$weston_log\" || report_weston_failure drm_backend",
            smoke,
        )

    def test_shm_pointer_listener_handles_every_event_in_bound_version_five(self) -> None:
        client = self.read("q35-wayland-shm-client.c")
        self.assertIn("version < 5 ? version : 5", client)
        listener = client.split("static const struct wl_pointer_listener pointer_listener =", 1)[1].split("};", 1)[0]
        for event in ("enter", "leave", "motion", "button", "axis", "frame", "axis_source", "axis_stop", "axis_discrete"):
            with self.subTest(event=event):
                self.assertIn(f".{event} = pointer_{event}", listener)

    def test_shm_client_creates_buffers_in_the_private_runtime_directory(self) -> None:
        client = self.read("q35-wayland-shm-client.c")
        self.assertIn('getenv("XDG_RUNTIME_DIR")', client)
        self.assertIn('!runtime_dir || !*runtime_dir', client)
        self.assertIn('"%s/thekernel-wl-shm-XXXXXX", runtime_dir', client)
        self.assertIn('(size_t)length >= sizeof(name)', client)
        self.assertNotIn('char name[] = "/thekernel-wl-shm-', client)
        self.assertIn('unlink(name)', client)

    def test_guest_uapi_oracles_fail_loudly_and_run_before_the_ready_marker(self) -> None:
        smoke = self.read("overlay/common/usr/local/bin/graphics-abi-smoke")
        self.assertLess(smoke.index("/usr/local/bin/drm-uapi-oracle"), smoke.index("echo 'THEKERNEL_GRAPHICS_ABI_SMOKE_READY'"))
        self.assertLess(smoke.index("/usr/local/bin/evdev-uapi-oracle"), smoke.index("echo 'THEKERNEL_GRAPHICS_ABI_SMOKE_READY'"))
        q35_smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertLess(q35_smoke.index("/usr/local/bin/drm-uapi-oracle"), q35_smoke.index("q35-wayland-shm-client"))
        self.assertLess(q35_smoke.index("/usr/local/bin/evdev-uapi-oracle"), q35_smoke.index("q35-wayland-shm-client"))
        drm_oracle = (ROOT / "tests/guest/graphics/drm-uapi-oracle.c").read_text()
        self.assertIn('strcmp(state, "FAIL") == 0', drm_oracle)
        self.assertIn("return failures == 0 ? 0 : 1", drm_oracle)
        evdev_oracle = (ROOT / "tests/guest/graphics/evdev-uapi-oracle.c").read_text()
        self.assertIn('result("evdev.open", "FAIL"', evdev_oracle)
        self.assertIn("bit_bytes == (int)sizeof(unsigned long)", evdev_oracle)
        self.assertIn("return failures == 0 ? 0 : 1", evdev_oracle)

    def test_graphics_smoke_hands_an_existing_rootfs_to_the_drive_transport_without_building(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        with test_tmpdir() as directory:
            rootfs = pathlib.Path(directory) / "graphics-rootfs.ext2"
            rootfs.write_bytes(b"rootfs")
            screenshot = pathlib.Path(directory) / "graphics.ppm"
            args = module.build_parser().parse_args([
                "test", "--suite", "graphics", "--no-build", "--rootfs", str(rootfs),
                "--screenshot", str(screenshot),
            ])
            calls: dict[str, object] = {}
            module.build_kernel = lambda *_args, **_kwargs: self.fail("--no-build invoked build_kernel")
            module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertEqual(spec.rootfs, rootfs.resolve())
        self.assertEqual(spec.rootfs_transport, "drive")

    def test_piglit_desktop_gl_contract_keeps_complete_tests_and_uses_buildroots_cli(self) -> None:
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertIn(
            "start-stop-daemon -S -x /usr/local/bin/q35-virgl-workloads -c weston",
            smoke.splitlines(),
        )
        piglit = self.read("overlay/q35-software-desktop/usr/local/bin/q35-piglit-quick")
        self.assertIn("runner=/usr/bin/piglit", piglit)
        self.assertIn('-p wayland -1 --timeout 60 quick "$results"', piglit)
        self.assertIn("q35-piglit-result-check", piglit)
        self.assertNotIn("piglit-runner", piglit)

    def test_software_graphics_profiles_use_softpipe_without_triggering_buildroot_legacy(self) -> None:
        profiles = (
            "common.config",
            "q35-software-desktop.fragment",
            "q35-graphics-seatd.fragment",
            "q35-graphics-benchmark.fragment",
        )
        for profile in profiles:
            contents = self.read(profile)
            self.assertIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE=y", contents)
            self.assertNotIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SWRAST", contents)
            self.assertNotIn("BR2_LEGACY=y", contents)

    def test_logind_preflight_accepts_indented_vt_handoff_commands(self) -> None:
        cycle = self.read("overlay/q35-graphics-logind/usr/local/bin/thekernel-logind-cycle")
        session = self.read("overlay/q35-graphics-logind/usr/local/bin/thekernel-sway-session")
        self.assertIn('    chvt "$tty"', cycle)
        self.assertIn('    loginctl activate "$session"', cycle)
        self.assertIn('while [ "$cycle" -lt 1 ]', cycle)
        self.assertNotIn("inactive_fd_not_revoked", cycle)
        self.assertIn('rotate_lease "$active_state"', cycle)
        self.assertIn('lease_request "$active_old_dir"', cycle)
        self.assertIn('lease_request "$fresh_lease_dir"', cycle)
        self.assertIn("start_lease()", session)
        self.assertIn('lease.current.next', session)
        subprocess.run(
            [
                ROOT / "scripts/build-graphics-rootfs.sh",
                "--flavor",
                "q35-graphics-logind",
                "--check",
            ],
            cwd=ROOT,
            check=True,
        )

    def test_buildroot_version_accepts_exported_assignment_and_rejects_mismatch(self) -> None:
        with test_tmpdir() as directory:
            work = pathlib.Path(directory)
            source = work / "buildroot"
            source.mkdir()
            for declaration, accepted in (
                ("BR2_VERSION = 2026.05.2", True),
                ("  export BR2_VERSION := 2026.05.2", True),
                ("export BR2_VERSION := 2026.05.1", False),
                ("# BR2_VERSION is absent", False),
            ):
                with self.subTest(declaration=declaration):
                    (source / "Makefile").write_text(declaration + "\n")
                    # Stop at the next preflight check so no package build runs.
                    result = subprocess.run([
                        ROOT / "scripts/build-graphics-rootfs.sh",
                        "--flavor", "q35-software-desktop",
                        "--buildroot-dir", str(source),
                        "--output", str(work / "output"),
                        "--download-dir", str(work / "downloads"),
                        "--host-deps-dir", str(work / "missing-host-deps"),
                    ], cwd=ROOT, capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    if accepted:
                        self.assertIn("local Buildroot Perl modules unavailable", result.stderr)
                        self.assertNotIn("Buildroot version mismatch", result.stderr)
                    else:
                        self.assertIn("Buildroot version mismatch", result.stderr)

    def test_product_cli_accepts_an_existing_graphics_rootfs(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "run", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
        ])
        self.assertEqual(args.rootfs, "/tmp/graphics-rootfs.ext2")

    def test_graphics_smoke_configures_one_marker_for_qmp_and_stop(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "test", "--suite", "graphics", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        with mock.patch.object(pathlib.Path, "is_file", lambda _self: True):
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertEqual(spec.stop_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(spec.graphics_profile, "headless")
        self.assertEqual(spec.rootfs_transport, "drive")

    def test_q35_headless_graphics_smoke_keeps_the_software_marker_and_pixel_oracle(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "test", "--suite", "graphics", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-graphics-seatd",
            "--graphics-profile", "headless",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        with mock.patch.object(pathlib.Path, "is_file", lambda _self: True):
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertIsNone(spec.stop_after_marker)
        self.assertEqual(spec.completion_after_shutdown, "THEKERNEL_Q35_SOFTWARE_SMOKE_COMPLETE")
        self.assertEqual(len(spec.qmp_checkpoints), 5)
        self.assertIsNotNone(spec.qmp_checkpoints[-2].screenshot)
        self.assertEqual(spec.qmp_checkpoints[-1].input_events[0][0]["data"]["key"]["data"], "f12")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_Q35_WESTON_READY")
        self.assertEqual(spec.qmp_screenshot_size, (800, 600))
        self.assertEqual(spec.qmp_screenshot_color_blocks[0].rgb, (255, 0, 0))

    def test_graphics_smoke_forwards_gdb_without_losing_input_checkpoints(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "test", "--suite", "graphics", "--no-build", "--gdb",
            "--rootfs", "/unused/rootfs.ext2", "--screenshot", "/unused/frame.ppm",
            "--flavor", "q35-graphics-seatd", "--graphics-profile", "headless",
        ])
        with mock.patch.object(pathlib.Path, "is_file", return_value=True), \
                mock.patch.object(module, "run_product", return_value=0) as run_product:
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = run_product.call_args.args[1]
        self.assertTrue(spec.gdb)
        self.assertEqual(len(spec.qmp_checkpoints), 5)

    def test_virgl_profiles_keep_the_same_full_marker_and_pixel_oracle(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        for profile in ("virgl-interactive", "virgl-headless"):
            with self.subTest(profile=profile):
                args = module.build_parser().parse_args([
                    "test", "--suite", "graphics", "--no-build", "--rootfs", "/unused/rootfs.ext2",
                    "--screenshot", "/unused/graphics.ppm", "--flavor", "q35-graphics-seatd",
                    "--graphics-profile", profile, "--timeout", "7200",
                ])
                with mock.patch.object(pathlib.Path, "is_file", return_value=True), \
                        mock.patch.object(module, "run_product", return_value=0) as run_product:
                    self.assertEqual(module.graphics_smoke_cmd(args), 0)
                spec = run_product.call_args.args[1]
                self.assertEqual(spec.stop_after_marker, "THEKERNEL_Q35_VIRGL_READY")
                self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_Q35_VIRGL_READY")
                self.assertEqual(spec.qmp_screenshot_size, (800, 600))
                self.assertEqual((spec.graphics_width, spec.graphics_height), (800, 600))
                self.assertEqual(spec.qmp_screenshot_color_blocks[0].rgb, (255, 0, 0))
                self.assertEqual(spec.qmp_timeout_secs, 7200)
                self.assertEqual(spec.qmp_checkpoints, module._xwayland_smoke_checkpoints(pathlib.Path(args.screenshot)))

    def test_software_smoke_requires_completion_and_natural_shutdown(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        with test_tmpdir() as directory:
            root = pathlib.Path(directory)
            artifacts = module.Artifacts(root / "state", module.Variant(memory="128M"))
            for path in (artifacts.kernel, artifacts.esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            spec = module.RunSpec(
                accel="tcg", timeout=30, workdir=root / "run", interactive=False,
                input_after_marker=None, stop_after_marker=None,
                completion_after_shutdown="THEKERNEL_Q35_SOFTWARE_SMOKE_COMPLETE",
                commands=None, extra_block=None, run_cpus=1,
            )
            for clean, marker, code, expected in (
                (True, True, 0, 0), (True, False, 0, 1),
                (False, True, 0, 1), (False, True, -9, -9),
            ):
                with self.subTest(clean=clean, marker=marker, code=code):
                    def fake_run(config):
                        config.log_path.write_text(
                            "THEKERNEL_Q35_SOFTWARE_SMOKE_COMPLETE\n" if marker else "",
                            encoding="utf-8",
                        )
                        return type("Result", (), {
                            "returncode": code, "error_message": None,
                            "log_path": config.log_path, "guest_clean_shutdown": clean,
                            "diagnostic_log_path": config.workdir / "kernel.log",
                        })()
                    module.run = fake_run
                    self.assertEqual(module.run_product.__wrapped__(artifacts, spec), expected)

    def test_marker_gated_screenshot_is_forwarded_to_qemu_runner(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        with test_tmpdir() as directory:
            root = pathlib.Path(directory)
            artifacts = module.Artifacts(root / "state", module.Variant(memory="128M"))
            for path in (artifacts.kernel, artifacts.esp):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            rootfs = root / "graphics.ext2"
            rootfs.write_bytes(b"rootfs")
            screenshot = root / "graphics.ppm"
            seen: dict[str, object] = {}

            def fake_run(config):
                seen["config"] = config
                return type("Result", (), {
                    "returncode": 75, "error_message": None, "log_path": config.log_path,
                    "intentionally_stopped": True,
                    "guest_clean_shutdown": False,
                    "diagnostic_log_path": config.workdir / "kernel.log",
                })()

            module.run = fake_run
            self.assertEqual(module.run_product.__wrapped__(
                artifacts,
                module.RunSpec(
                    accel="tcg", timeout=30, workdir=root / "run", interactive=False,
                    input_after_marker=None, stop_after_marker="THEKERNEL_GRAPHICS_ABI_SMOKE_READY",
                    commands=None, extra_block=None, rootfs=rootfs, qmp_screenshot=screenshot,
                    qmp_screenshot_after_marker="THEKERNEL_GRAPHICS_ABI_SMOKE_READY",
                    run_cpus=1,
                ),
            ), 0)
        config = seen["config"]
        self.assertEqual(config.qmp.screenshot, screenshot.resolve())
        self.assertEqual(config.qmp.screenshot_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(config.qmp.socket.name, "graphics-smoke.qmp")


if __name__ == "__main__":
    unittest.main()
