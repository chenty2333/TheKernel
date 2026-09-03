#!/usr/bin/env python3
"""Static contract tests for the non-logind graphics userspace session."""
from __future__ import annotations

import pathlib
import importlib.util
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
GRAPHICS = ROOT / "config" / "graphics"


class GraphicsRootfsConfigTests(unittest.TestCase):
    def read(self, relative: str) -> str:
        return (GRAPHICS / relative).read_text()

    def test_busybox_init_orders_udev_before_seatd_before_weston(self) -> None:
        common = self.read("common.config")
        self.assertIn("BR2_INIT_BUSYBOX=y", common)
        self.assertIn("BR2_TARGET_ROOTFS_EXT2=y", common)
        self.assertIn(
            'BR2_TARGET_ROOTFS_EXT2_MKFS_OPTIONS="-O ^64bit,^metadata_csum_seed,^orphan_file"',
            common,
        )
        self.assertNotIn("BR2_ROOTFS_EXT2=y", common)
        self.assertNotIn("BR2_INIT_NONE=y", common)
        init = GRAPHICS / "overlay/common/etc/init.d"
        self.assertTrue((init / "S70seatd").is_file())
        self.assertTrue((init / "S80weston").is_file())
        self.assertLess("S10udevd", "S70seatd")
        self.assertLess("S70seatd", "S80weston")
        self.assertIn("BR2_KERNEL_HEADERS_6_12=y", common)

    def test_wrapper_uses_host_perl_unmodified_unless_a_prefix_is_explicit(self) -> None:
        wrapper = (ROOT / "scripts/build-graphics-rootfs.sh").read_text()
        self.assertIn('if [ -n "$host_deps_dir" ]; then', wrapper)
        self.assertLess(
            wrapper.index('if [ -n "$perl_module_root" ]; then'),
            wrapper.index('if [ -z "$host_deps_dir" ]; then host_deps_dir='),
        )
        self.assertIn('export PATH="$host_deps_dir/bin:$PATH"', wrapper)

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
                self.assertEqual(link.readlink(), pathlib.Path(expected), flavor)
        for flavor in manifest["SMOKE_FLAVORS"].split():
            self.assertIn(flavor, flavors)
        for flavor in manifest["CI_CHECK_FLAVORS"].split():
            self.assertIn(flavor, flavors)

    def test_seatd_socket_and_device_policy_are_auditable(self) -> None:
        seatd = self.read("overlay/common/etc/init.d/S70seatd")
        rules = self.read("overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules")
        self.assertIn("-g seat", seatd)
        self.assertIn('KERNEL=="card[0-9]*", GROUP="video", MODE="0660"', rules)
        self.assertIn('KERNEL=="renderD[0-9]*", GROUP="render", MODE="0660"', rules)
        self.assertIn('KERNEL=="fb[0-9]*", GROUP="video", MODE="0660"', rules)
        self.assertIn('KERNEL=="event[0-9]*", ENV{ID_SEAT}="seat0", ENV{WL_SEAT}="default", GROUP="input", MODE="0660"', rules)
        self.assertNotIn('KERNEL=="fb[0-9]*", GROUP="video", MODE="0666"', rules)
        self.assertIn("while [ ! -S /run/seatd.sock ]", seatd)

    def test_compositor_is_nonroot_and_uses_private_runtime_directory(self) -> None:
        users = self.read("users.table")
        init = self.read("overlay/common/etc/init.d/S80weston")
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertIn("weston -1 weston -1 !* /var/lib/weston /bin/sh seat,render", users)
        self.assertIn('-c "$USER"', init)
        self.assertIn("chmod 0700", init)
        self.assertIn('export XDG_RUNTIME_DIR="$runtime_dir"', session)
        self.assertIn("export LIBSEAT_BACKEND=seatd", session)
        self.assertIn('[ "$(id -u)" -ne 0 ]', session)

    def test_q35_runtime_proves_the_selected_libseat_backend(self) -> None:
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertIn("stat -c %U:%G /run/seatd.sock", smoke)
        self.assertIn("stat -c %a /run/seatd.sock", smoke)
        self.assertIn("LIBSEAT_BACKEND=seatd", smoke)
        self.assertIn("Seat opened with backend 'seatd'", smoke)
        self.assertIn("ID_SEAT=seat0", smoke)
        self.assertIn("WL_SEAT=default", smoke)
        self.assertIn("/run/udev/data/c", smoke)

    def test_headless_init_profile_selects_headless_backend_without_a_login_shell(self) -> None:
        common = self.read("common.config")
        init = self.read("overlay/common/etc/init.d/S80weston")
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertNotIn("SYSTEMD_LOGIND=y", common)
        self.assertEqual(
            self.read("overlay/headless/etc/thekernel-graphics-flavor").strip(),
            "headless-abi-smoke",
        )
        self.assertNotIn("FLAVOR_FILE", init)
        self.assertNotIn("THEKERNEL_GRAPHICS_FLAVOR", init)
        self.assertNotIn('"$flavor"', init)
        link = GRAPHICS / "overlay/headless/etc/weston/weston.ini"
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.readlink(), pathlib.Path("weston-headless.ini"))
        self.assertIn("exec weston --config=/etc/weston/weston.ini", session)

    def test_every_graphics_flavor_cross_compiles_the_guest_uapi_oracles(self) -> None:
        headless = self.read("headless-abi-smoke.fragment")
        q35 = self.read("q35-graphics-seatd.fragment")
        build = self.read("build-guest-tools.sh")
        smoke = self.read("overlay/common/usr/local/bin/graphics-abi-smoke")
        self.assertIn('BR2_ROOTFS_POST_BUILD_SCRIPT="@REPO_ROOT@/config/graphics/build-guest-tools.sh"', headless)
        self.assertIn("build-guest-tools.sh", q35)
        self.assertIn("build-q35-wayland-client.sh", q35)
        self.assertIn("tests/guest/graphics/drm-uapi-oracle.c", build)
        self.assertIn("tests/guest/graphics/evdev-uapi-oracle.c", build)
        self.assertIn('"$STAGING_DIR/usr/include/libdrm"', build)
        self.assertIn("/usr/local/bin/drm-uapi-oracle", build)
        self.assertIn("/usr/local/bin/evdev-uapi-oracle", build)
        self.assertLess(smoke.index("/usr/local/bin/drm-uapi-oracle"), smoke.index("THEKERNEL_GRAPHICS_ABI_SMOKE_READY"))
        self.assertLess(smoke.index("/usr/local/bin/evdev-uapi-oracle"), smoke.index("THEKERNEL_GRAPHICS_ABI_SMOKE_READY"))
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
        spec = importlib.util.spec_from_file_location("thekernel_graphics_no_stale_rootfs", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            rootfs = pathlib.Path(directory) / "graphics-rootfs.ext2"
            rootfs.write_bytes(b"rootfs")
            screenshot = pathlib.Path(directory) / "graphics.ppm"
            args = module.build_parser().parse_args([
                "graphics-smoke", "--no-build", "--rootfs", str(rootfs),
                "--screenshot", str(screenshot),
            ])
            calls: dict[str, object] = {}
            module.build_kernel = lambda *_args, **_kwargs: self.fail("--no-build invoked build_kernel")
            module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertEqual(spec.rootfs, rootfs.resolve())
        self.assertEqual(spec.rootfs_transport, "drive")

    def test_canonical_q35_seatd_profile_selects_drm_backend_without_a_login_shell(self) -> None:
        init = self.read("overlay/common/etc/init.d/S80weston")
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertEqual(
            self.read("overlay/q35-graphics-seatd/etc/thekernel-graphics-flavor").strip(),
            "q35-graphics-seatd",
        )
        self.assertNotIn('"$flavor"', init)
        link = GRAPHICS / "overlay/q35-graphics-seatd/etc/weston/weston.ini"
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.readlink(), pathlib.Path("weston-drm.ini"))
        self.assertIn("exec weston --config=/etc/weston/weston.ini", session)

    def test_q35_smoke_sets_the_weston_session_environment_before_dropping_privileges(self) -> None:
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertIn("export HOME=/var/lib/weston", smoke)
        self.assertIn("export XDG_RUNTIME_DIR=/run/user/$weston_uid", smoke)
        self.assertIn("export WAYLAND_DISPLAY=wayland-0", smoke)
        self.assertIn('stat -c %u "$XDG_RUNTIME_DIR"', smoke)
        self.assertIn('[ "$(stat -c %a "$XDG_RUNTIME_DIR")" = 700 ]', smoke)
        self.assertIn("runtime_dir_is_private ||", smoke)
        self.assertIn('start-stop-daemon -S -x /usr/local/bin/q35-wayland-shm-client -c weston -b', smoke)

    def test_q35_virgl_smoke_requires_the_render_node_and_forces_the_driver(self) -> None:
        fragment = self.read("q35-graphics-seatd.fragment")
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        workloads = self.read("overlay/q35-software-desktop/usr/local/bin/q35-virgl-workloads")
        client = self.read("q35-wayland-color-client.c")
        build = self.read("build-q35-wayland-client.sh")
        self.assertIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE=y", fragment)
        self.assertIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y", fragment)
        self.assertIn("/dev/dri/renderD128", smoke)
        self.assertIn('if [ ! -c /dev/dri/renderD128 ]; then', smoke)
        self.assertIn('start-stop-daemon -S -x /usr/local/bin/q35-wayland-shm-client -c weston -b', smoke)
        self.assertIn('start-stop-daemon -S -x /usr/local/bin/q35-virgl-workloads -c weston -b', smoke)
        self.assertIn("MESA_LOADER_DRIVER_OVERRIDE=virgl", workloads)
        self.assertIn("THEKERNEL_Q35_VIRGL_READY", workloads)
        self.assertIn("THEKERNEL_Q35_RENDER_MARKER", client)
        self.assertIn(
            "THEKERNEL_Q35_RENDER_MARKER=THEKERNEL_Q35_VIRGL_GLES_READY",
            workloads,
        )
        self.assertIn("wl_surface_frame", client)
        self.assertLess(client.index("wl_surface_frame"), client.index("eglSwapBuffers"))
        self.assertNotIn("wl_surface_commit(a->s); return 0", client)
        self.assertIn("-Wl,-z,defs", build)
        self.assertIn("-lwayland-egl", build)
        self.assertIn("-lEGL", build)
        self.assertIn("-lGLESv2", build)

    def test_piglit_desktop_gl_contract_keeps_complete_tests_and_uses_buildroots_cli(self) -> None:
        seatd = self.read("q35-graphics-seatd.fragment")
        software = self.read("q35-software-desktop.fragment")
        piglit = self.read("overlay/q35-software-desktop/usr/local/bin/q35-piglit-quick")
        for fragment in (seatd, software):
            self.assertIn('BR2_TARGET_ROOTFS_EXT2_SIZE="3G"', fragment)
            self.assertIn("BR2_PACKAGE_MESA3D_OPENGL_GLX=y", fragment)
            self.assertIn("BR2_PACKAGE_PIGLIT=y", fragment)
            self.assertIn("BR2_PACKAGE_XORG7=y", fragment)
        for fragment in (seatd, software):
            self.assertIn("BR2_PACKAGE_LIBEPOXY=y", fragment)
            self.assertIn("BR2_PACKAGE_WESTON_XWAYLAND=y", fragment)
        drm_ini = self.read("overlay/q35-software-desktop/etc/weston/weston-drm.ini")
        self.assertIn("xwayland=true", drm_ini)
        self.assertIn("runner=/usr/bin/piglit", piglit)
        self.assertIn('-p wayland -1 --timeout 60 quick "$results"', piglit)
        self.assertIn("q35-piglit-result-check", piglit)
        self.assertNotIn("piglit-runner", piglit)
        wrapper = (ROOT / "scripts/build-graphics-rootfs.sh").read_text()
        self.assertIn('"$target/usr/lib/piglit/tests/quick.meta.xml"', wrapper)
        self.assertIn('BR2_PACKAGE_LIBEPOXY=y', wrapper)
        self.assertIn('BR2_PACKAGE_WESTON_XWAYLAND=y', wrapper)
        self.assertIn('q35-piglit-result-check', wrapper)

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

    def test_benchmark_modeset_fault_uses_kms_and_keeps_vt_switch_separate(self) -> None:
        benchmark = self.read("overlay/q35-graphics-benchmark/etc/init.d/S90q35-graphics-benchmark")
        helper = self.read("q35-drm-modeset-restore.c")
        build = self.read("build-q35-graphics-benchmark-tools.sh")
        self.assertIn("modeset) modeset_fault", benchmark)
        self.assertIn("/etc/init.d/S80weston stop", benchmark)
        self.assertIn("/etc/init.d/S80weston start", benchmark)
        self.assertIn("vt-switch) chvt 1; sleep 1; chvt 2", benchmark)
        self.assertIn("DRM_IOCTL_MODE_SETCRTC", helper)
        self.assertIn("connected connector with two distinct modes", helper)
        self.assertIn("q35-drm-modeset-restore", build)

    def test_benchmark_hotplug_waits_for_remove_eudev_and_weston_libinput(self) -> None:
        benchmark = self.read("overlay/q35-graphics-benchmark/etc/init.d/S90q35-graphics-benchmark")
        self.assertIn("THEKERNEL_GRAPHICS_INPUT_HOTPLUG_REMOVED", benchmark)
        self.assertIn("THEKERNEL_GRAPHICS_INPUT_HOTPLUG_READY", benchmark)
        self.assertIn("start_hotplug_observer", benchmark)
        self.assertLess(benchmark.index("input-hotplug) start_hotplug_observer"), benchmark.rindex("THEKERNEL_GRAPHICS_BENCHMARK=1"))
        self.assertIn("[ ! -e \"$old_event\" ]", benchmark)
        self.assertIn("udevadm settle --timeout=1", benchmark)
        self.assertIn("/run/udev/data/c$((0x$major_hex)):$((0x$minor_hex))", benchmark)
        self.assertIn("weston_uses_input", benchmark)

    def test_benchmark_selects_real_software_virgl_and_venus_workloads(self) -> None:
        fragment = self.read("q35-graphics-benchmark.fragment")
        benchmark = self.read("overlay/q35-graphics-benchmark/etc/init.d/S90q35-graphics-benchmark")
        build = self.read("build-q35-graphics-benchmark-tools.sh")
        self.assertIn("BR2_PACKAGE_MESA3D_VULKAN_DRIVER_VIRTIO=y", fragment)
        self.assertIn("BR2_PACKAGE_VULKAN_LOADER=y", fragment)
        self.assertIn("BR2_PACKAGE_VULKAN_TOOLS=y", fragment)
        self.assertIn('BR2_TARGET_ROOTFS_EXT2_SIZE="512M"', fragment)
        self.assertIn("q35-wayland-egl-benchmark.c", build)
        self.assertIn("q35-wayland-vulkan-benchmark.c", build)
        self.assertIn("q35-wayland-vulkan-benchmark-client", benchmark)
        self.assertIn("q35-wayland-egl-benchmark-client", benchmark)
        self.assertIn("q35-wayland-benchmark-client", benchmark)
        self.assertIn("vulkaninfo --summary", benchmark)
        self.assertIn("q35-virgl-render-oracle", benchmark)
        self.assertIn("LIBGL_ALWAYS_SOFTWARE=0", benchmark)

    def test_benchmark_post_build_resolves_sources_beside_the_script(self) -> None:
        build = self.read("build-q35-graphics-benchmark-tools.sh")
        self.assertIn('base_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)', build)
        for source in (
            "q35-wayland-egl-benchmark.c",
            "q35-wayland-vulkan-benchmark.c",
            "q35-drm-modeset-restore.c",
        ):
            self.assertIn(f'"$base_dir/{source}"', build)
        self.assertNotIn("$source_dir/config/graphics/", build)

    def test_benchmark_fault_crashes_the_selected_renderer_after_ready(self) -> None:
        benchmark = self.read("overlay/q35-graphics-benchmark/etc/init.d/S90q35-graphics-benchmark")
        egl = self.read("q35-wayland-egl-benchmark.c")
        vulkan = self.read("q35-wayland-vulkan-benchmark.c")
        shm = self.read("q35-wayland-shm-client.c")
        self.assertLess(benchmark.index("printf 'THEKERNEL_GRAPHICS_RENDERER"), benchmark.index('if [ "$fault" = client-crash ]'))
        self.assertIn("client_crash_not_ready", benchmark)
        self.assertIn('grep -qx \'THEKERNEL_GRAPHICS_BENCHMARK_READY\'', benchmark)
        for source in (egl, vulkan, shm):
            self.assertIn("abort();", source)

    def test_benchmark_input_marker_has_a_causal_pixel_state(self) -> None:
        egl = self.read("q35-wayland-egl-benchmark.c")
        vulkan = self.read("q35-wayland-vulkan-benchmark.c")
        shm = self.read("q35-wayland-shm-client.c")
        self.assertIn("app->input_state = !app->input_state", egl)
        self.assertIn("app->input_state ? 0.9f : 0.1f", egl)
        self.assertIn("app->input_state = !app->input_state", vulkan)
        self.assertIn("app->input_state ? 0.9f : 0.1f", vulkan)
        self.assertIn("a->benchmark_accent = (a->input_sequence & 1)", shm)

    def test_weston_stop_waits_for_the_old_drm_master_before_removing_its_pidfile(self) -> None:
        init = self.read("overlay/common/etc/init.d/S80weston")
        self.assertIn("wait_for_exit", init)
        self.assertIn("kill -TERM", init)
        self.assertIn("kill -KILL", init)
        self.assertLess(init.index("wait_for_exit \"$pid\""), init.rindex('rm -f "$PIDFILE"'))

    def test_q35_smoke_closes_the_eudev_input_classification_chain(self) -> None:
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertIn('udevadm info -q property -n "$event"', smoke)
        self.assertIn("ID_INPUT=1", smoke)
        self.assertIn("ID_INPUT_(MOUSE|TABLET|TOUCHSCREEN)=1", smoke)
        self.assertIn("phase=input_classification", smoke)
        self.assertIn("udevadm settle --timeout=10", smoke)
        self.assertIn("have_keyboard", smoke)
        self.assertIn("have_pointer", smoke)

    def test_wrapper_checks_exact_graphics_group_membership(self) -> None:
        wrapper = (ROOT / "scripts/build-graphics-rootfs.sh").read_text()
        self.assertIn("grep -Eq '^seat:[^:]*:[^:]*:([^,]+,)*weston(,[^,]+)*$'", wrapper)
        self.assertIn("grep -Eq '^render:[^:]*:[^:]*:([^,]+,)*weston(,[^,]+)*$'", wrapper)

    def test_wrapper_validates_the_mesa_26_unified_gallium_layout(self) -> None:
        wrapper = (ROOT / "scripts/build-graphics-rootfs.sh").read_text()
        self.assertIn('"$target/usr/lib/gbm/dri_gbm.so"', wrapper)
        self.assertIn("-name 'libgallium-*.so'", wrapper)
        self.assertNotIn("virtio_gpu_dri.so", wrapper)
        self.assertNotIn("kms_swrast_dri.so", wrapper)

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

    def test_ci_hands_the_graphics_rootfs_to_the_pixel_oracle(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("needs: image_ref", workflow)
        self.assertIn("image: ${{ needs.image_ref.outputs.image }}", workflow)
        graphics_step = workflow[workflow.index("Run canonical Q35 seatd Pixman pixel oracle"):]
        graphics_step = graphics_step.split("panther-lake-dut:", 1)[0]
        self.assertNotIn("--no-build", graphics_step)
        self.assertIn("--flavor q35-graphics-seatd", graphics_step)
        self.assertIn('--rootfs "$graphics_state/q35-graphics-seatd/images/rootfs.ext2"', graphics_step)

    def test_product_cli_accepts_an_existing_graphics_rootfs(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_graphics_cli", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "run", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
        ])
        self.assertEqual(args.rootfs, "/tmp/graphics-rootfs.ext2")

    def test_graphics_smoke_configures_one_marker_for_qmp_and_stop(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_graphics_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        finally:
            pathlib.Path.is_file = original_is_file
        spec = calls["spec"]
        self.assertEqual(spec.stop_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(spec.graphics_profile, "headless")
        self.assertEqual(spec.rootfs_transport, "drive")

    def test_q35_headless_graphics_smoke_keeps_the_software_marker_and_pixel_oracle(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_q35_headless_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-graphics-seatd",
            "--graphics-profile", "headless",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        finally:
            pathlib.Path.is_file = original_is_file
        spec = calls["spec"]
        self.assertEqual(spec.stop_after_marker, "THEKERNEL_Q35_WESTON_READY")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_Q35_WESTON_READY")
        self.assertEqual(spec.qmp_screenshot_size, (800, 600))
        self.assertEqual(spec.qmp_screenshot_color_blocks[0].rgb, (255, 0, 0))

    def test_virgl_headless_graphics_smoke_is_rejected_without_a_qmp_pixel_oracle(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_virgl_headless_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-graphics-seatd",
        ])
        args.graphics_profile = "virgl-headless"
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            with self.assertRaisesRegex(module.ProductError, "no QMP pixel-oracle surface"):
                module.graphics_smoke_cmd(args)
        finally:
            pathlib.Path.is_file = original_is_file

    def test_virgl_graphics_smoke_uses_the_virgl_marker_and_pixel_oracle(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_virgl_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-graphics-seatd",
            "--graphics-profile", "virgl-interactive",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        finally:
            pathlib.Path.is_file = original_is_file
        spec = calls["spec"]
        self.assertEqual(spec.stop_after_marker, "THEKERNEL_Q35_VIRGL_READY")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_Q35_VIRGL_READY")
        self.assertEqual(spec.qmp_screenshot_size, (800, 600))
        self.assertEqual(spec.qmp_screenshot_color_blocks[0].rgb, (255, 0, 0))

    def test_marker_gated_screenshot_is_forwarded_to_qemu_runner(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_graphics_qmp", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
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
                })()

            module.run = fake_run
            self.assertEqual(module.run_product(
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
