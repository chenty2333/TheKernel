"""Focused product-system-test completion and KTAP gate tests."""

from __future__ import annotations

import hashlib
import os
import re
import subprocess
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
    def test_guest_tool_paths_match_installed_source_names(self) -> None:
        # Crate/package renames must not rename the independently installed C tools.
        installed = {f"thekernel-{path.stem}"
                     for path in (REPO_ROOT / "tests/guest/tools").glob("*.c")}
        source = (REPO_ROOT / "tests/guest/system-init.c").read_text()
        referenced = set(re.findall(r'"/opt/thekernel-tests/bin/([^"/]+)"', source))
        self.assertTrue(referenced)
        self.assertEqual(referenced - installed, set())

    def test_io_submit_batch_is_independent_and_uses_separate_artifacts(self) -> None:
        product = load_product()
        artifacts = []
        for flags in ([], ["--io-submit-batch"], ["--m5-candidate"],
                      ["--m5-candidate", "--io-submit-batch"]):
            args = product.build_parser().parse_args(["build", "--profile", "shell", *flags])
            artifacts.append(product.Artifacts(Path("/unused"), product.parse_variant(args), args.profile))
        self.assertEqual(len({item.output_dir for item in artifacts}), 4)
        self.assertEqual(len({item.cargo_target_dir for item in artifacts}), 4)
        self.assertEqual(product.kernel_features(artifacts[0]), "x86-product boot-shell")
        self.assertEqual(product.kernel_features(artifacts[1]), "x86-product boot-shell io-submit-batch")
        self.assertEqual(product.kernel_features(artifacts[2]), product.kernel_features(artifacts[3]))
        self.assertEqual(product.kernel_features(artifacts[3]).split().count("io-submit-batch"), 1)

    def test_io_notify_fastpath_is_independent_and_uses_separate_artifacts(self) -> None:
        product = load_product()
        artifacts = []
        for flags in ([], ["--io-notify-fastpath"], ["--io-submit-batch"],
                      ["--io-submit-batch", "--io-notify-fastpath"]):
            args = product.build_parser().parse_args(["build", "--profile", "shell", *flags])
            artifacts.append(product.Artifacts(Path("/unused"), product.parse_variant(args), args.profile))
        self.assertEqual(len({item.output_dir for item in artifacts}), 4)
        self.assertEqual(len({item.cargo_target_dir for item in artifacts}), 4)
        self.assertEqual(product.kernel_features(artifacts[1]), "x86-product boot-shell io-notify-fastpath")
        self.assertEqual(product.kernel_features(artifacts[3]),
                         "x86-product boot-shell io-submit-batch io-notify-fastpath")

    def test_candidate_flags_cannot_replace_io_or_graphics_benchmark_baseline(self) -> None:
        product = load_product()
        for suite in ("io", "graphics"):
            for flag in ("--io-submit-batch", "--m5-candidate", "--io-notify-fastpath"):
                with self.subTest(suite=suite, flag=flag):
                    args = product.build_parser().parse_args(["bench", "--suite", suite, flag])
                    with patch.object(product, "graphics_benchmark_cmd") as graphics:
                        with self.assertRaisesRegex(product.ProductError, "baseline must use default"):
                            product.bench_cmd(args)
                    graphics.assert_not_called()

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

    def test_toolchain_flag_selects_the_payload_over_the_environment(self) -> None:
        """`--toolchain` must reach the artifact layout, not be echoed away.

        The regression this pins: main() used to write the *environment* value
        back into THEKERNEL_TOOLCHAIN, so an exported default silently replaced
        the flag and the guest suite built and booted the baseline image while
        reporting success.
        """

        product = load_product()
        for environment, flag, expected in (
            (None, "tcc", "tcc"),
            ("none", "tcc", "tcc"),
            ("tcc", "none", "none"),
            ("tcc", None, "tcc"),
        ):
            with self.subTest(environment=environment, flag=flag), \
                    test_tmpdir() as directory:
                argv = ["test", "--suite", "guest", "--no-build"]
                if flag is not None:
                    argv += ["--toolchain", flag]
                with patch.dict(os.environ, {"THEKERNEL_STATE_DIR": directory}, clear=False):
                    if environment is None:
                        os.environ.pop("THEKERNEL_TOOLCHAIN", None)
                    else:
                        os.environ["THEKERNEL_TOOLCHAIN"] = environment
                    with patch.object(product, "run_product", return_value=0) as run:
                        self.assertEqual(product.main(argv), 0)
                    self.assertEqual(os.environ["THEKERNEL_TOOLCHAIN"], expected)
                    rootfs = run.call_args.args[0].rootfs
                    self.assertEqual(
                        rootfs.name,
                        "rootfs-x86.img" if expected == "none"
                        else f"rootfs-x86-{expected}.img",
                    )
                    self.assertTrue(rootfs.is_relative_to(directory))

    def test_payload_selects_the_guest_suite_plan(self) -> None:
        """Each payload's image must carry exactly the cases it can run.

        The case table is the suite's plan, so it is a compile-time property of
        the image rather than something a run can discover.  An image built for
        a payload that is missing its tool therefore fails -- which is what
        makes "the payload works" a testable claim instead of a description.
        """

        source = REPO_ROOT / "tests" / "guest" / "system-init.c"
        defines = {
            "none": [],
            "tcc": ["-DTHEKERNEL_TOOL_PAYLOAD_TCC=1"],
            "nested": ["-DTHEKERNEL_TOOL_PAYLOAD_TCC=1",
                       "-DTHEKERNEL_TOOL_PAYLOAD_NESTED=1"],
            # `glibc` is a staging milestone and deliberately excludes the
            # compiler case: it is a separate payload with its own cost.
            "glibc": ["-DTHEKERNEL_TOOL_PAYLOAD_GLIBC=1"],
            # `gcc` is a superset: the compiler is a dynamic glibc program, so
            # the loader case runs in its image too and the image proves its own
            # prerequisite rather than assuming it.
            "gcc": ["-DTHEKERNEL_TOOL_PAYLOAD_GLIBC=1",
                    "-DTHEKERNEL_TOOL_PAYLOAD_GCC=1"],
        }
        expected = {
            "none": [],
            "tcc": ["compiler-smoke"],
            "nested": ["compiler-smoke", "nested-tcg-hello", "nested-linux-boot"],
            "glibc": ["glibc-smoke"],
            "gcc": ["glibc-smoke", "gcc-smoke"],
        }
        with test_tmpdir() as directory:
            for payload, flags in defines.items():
                with self.subTest(payload=payload):
                    out = Path(directory) / f"plan-{payload}.i"
                    result = subprocess.run(
                        ["gcc", "-E", "-std=c11", *flags, str(source)],
                        capture_output=True, text=True, check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    text = result.stdout
                    present = [name for name in ("compiler-smoke", "nested-tcg-hello",
                                                 "nested-linux-boot", "glibc-smoke",
                                                 "gcc-smoke")
                               if f'{{ "{name}",' in text]
                    self.assertEqual(present, expected[payload])
                    # Both payloads are supersets of `none`, so the baseline
                    # cases must survive every selection.
                    for baseline in ("jit-mem", "proc-shape", "threads-futex",
                                     "io-uring", "seccomp"):
                        self.assertIn(f'{{ "{baseline}",', text)

    def test_staged_nested_payload_satisfies_its_interface(self) -> None:
        """The staged payload must be usable by the guest it is copied into.

        Two of these were real bugs, not hypotheticals: a payload staged
        without QEMU's data directory boots nothing ("could not load PC BIOS
        'bios-256k.bin'"), and an emulator that acquires a dynamic loader or a
        glibc libatomic cannot run in the guest at all.  Rebuilding the payload
        is minutes of work, so the assembled result is checked here instead of
        being discovered by a boot that fails for an unrelated-looking reason.

        The payload is a build artifact, so this test skips when it has not
        been built rather than failing a checkout that never asked for it.
        """

        state = Path(os.environ.get(
            "THEKERNEL_STATE_DIR",
            Path.home() / ".cache" / "thekernel-targets"))
        payload = state / "guest-tools" / "nested"
        tools = payload / "opt" / "thekernel-tools"
        if not (tools / "MANIFEST").is_file():
            self.skipTest(f"the nested payload has not been staged in {payload}")

        qemu = tools / "bin" / "qemu-system-x86_64"
        kernel = tools / "payloads" / "hello-acpi.elf"
        inner_kernel = tools / "payloads" / "vmlinuz-virt"
        inner_initrd = tools / "payloads" / "alpine-initramfs.cpio.gz"
        share = tools / "share"
        for path in (qemu, kernel, inner_kernel, inner_initrd, share / "bios-256k.bin"):
            with self.subTest(path=path.name):
                self.assertTrue(path.is_file(), f"missing from the payload: {path}")

        # Every file the manifest names must exist with the recorded hash.
        listed = 0
        for line in (tools / "MANIFEST").read_text().splitlines():
            fields = line.split()
            if line.startswith("#") or len(fields) != 3:
                continue  # a comment or a header field, not a file record
            # The manifest records `path size sha256`, matching what
            # `sha256sum --check` wants once the fields are reordered.
            relative, size, digest = fields
            self.assertEqual(len(digest), 64, f"not a sha256: {line}")
            listed += 1
            with self.subTest(file=relative):
                staged = payload / relative
                self.assertTrue(staged.is_file())
                self.assertEqual(staged.stat().st_size, int(size))
                self.assertEqual(
                    hashlib.sha256(staged.read_bytes()).hexdigest(), digest)
        # The emulator, the two inner images and the Alpine initramfs.  A
        # manifest that lost an entry would otherwise let the payload shrink
        # silently and fail much later, inside a boot.
        self.assertEqual(listed, 4)

        # A dynamic emulator cannot start in the guest, and the guest has no
        # dynamic loader to give it.
        program_headers = subprocess.run(
            ["readelf", "-d", str(qemu)], capture_output=True, text=True, check=False)
        self.assertNotIn("NEEDED", program_headers.stdout)

        # glibc's libatomic must not be linked in.  The only libatomic
        # reachable on this host is Fedora's, and linking it into a musl binary
        # produces one that starts, runs, and then dies with no diagnostic.
        symbols = subprocess.run(
            ["nm", str(qemu)], capture_output=True, text=True, check=False)
        self.assertNotIn(" libat_", symbols.stdout)

        # The inner image is an ELF32 multiboot container by construction:
        # QEMU reloads -kernel images with I386_ELF_MACHINE.
        header = kernel.read_bytes()[:20]
        self.assertEqual(header[:4], b"\x7fELF")
        self.assertEqual(header[4], 1, "the inner image must be ELF32")

        # The Alpine image is a bzImage, which is what QEMU accepts for -kernel.
        # The Linux boot protocol puts "HdrS" at 0x202; checking it here means a
        # wrong or truncated download is caught without a 180 s boot attempt.
        self.assertEqual(inner_kernel.read_bytes()[0x202:0x206], b"HdrS")
        # A gzip-compressed cpio newc archive, which the kernel unpacks as the
        # initial root filesystem.
        initrd = inner_initrd.read_bytes()
        self.assertEqual(initrd[:2], b"\x1f\x8b", "the initramfs must be gzip")
        self.assertGreater(len(initrd), 100_000)

    def test_staged_glibc_payload_satisfies_its_interface(self) -> None:
        """The glibc payload must stage what a dynamic program needs.

        Checked here rather than only in the guest because the interesting
        failure -- a loader at the wrong path, or a smoke binary that is not
        actually dynamic -- is a staging bug that looks like a kernel bug when
        it surfaces two minutes later as an ENOENT inside the guest.
        """

        root = Path(os.environ.get(
            "THEKERNEL_STATE_DIR",
            Path.home() / ".cache" / "thekernel-targets",
        ))
        tools = root / "guest-tools" / "glibc"
        loader = tools / "lib64" / "ld-linux-x86-64.so.2"
        libc = tools / "lib64" / "libc.so.6"
        program = tools / "opt/thekernel-tools/bin/glibc-smoke"
        if not program.is_file():
            self.skipTest(f"no staged glibc payload at {tools}")

        # PT_INTERP is a fixed string in the executable and the kernel resolves
        # exactly it, so the staged path has to be that string.
        self.assertTrue(loader.is_file(), "the loader is not staged where PT_INTERP points")
        self.assertTrue(libc.is_file())
        header = program.read_bytes()[:64]
        self.assertEqual(header[:4], b"\x7fELF")
        self.assertEqual(header[4], 2, "the smoke program must be ELF64")
        text = program.read_bytes()
        self.assertIn(b"/lib64/ld-linux-x86-64.so.2\x00", text,
                      "the program's PT_INTERP does not name the staged loader")
        self.assertIn(b"libc.so.6\x00", text,
                      "the program has no DT_NEEDED for the staged libc")

        # The loader must be able to run first, so it cannot need a loader.
        self.assertNotIn(b"ld-linux-x86-64.so.2\x00", loader.read_bytes()[:4096],
                         "the staged loader looks like it has its own PT_INTERP")

    def test_image_reuse_follows_the_payload_it_embeds(self) -> None:
        """A rebuilt payload must invalidate the image built from it.

        This was a real bug: the reuse decision hashed the repository and the
        payload *name*, so a payload rebuilt to contain the compiler left the
        old image in place, and the extra case the image's own plan promised
        then failed inside the guest with a missing-compiler error that points
        at the compiler rather than at the image.
        """

        product_state = load_script_module(
            "thekernel_product_state_payload", "tools/product_state.py")
        with test_tmpdir() as directory:
            root = Path(directory)
            payload = product_state.guest_tools_dir(root, "nested")
            self.assertEqual(
                product_state.guest_tools_fingerprint(root, "nested"), "absent")

            (payload / "usr" / "bin").mkdir(parents=True)
            compiler = payload / "usr" / "bin" / "tcc"
            compiler.write_bytes(b"first")
            first = product_state.guest_tools_fingerprint(root, "nested")

            # The payload is rebuilt on every build, so every file gets a new
            # timestamp even when its bytes are unchanged.  That must not look
            # like a change, or every run would rebuild the image and the
            # before/after comparison inside the build could never agree.
            stat = compiler.stat()
            os.utime(compiler, ns=(stat.st_atime_ns, stat.st_mtime_ns + 1_000_000))
            self.assertEqual(
                product_state.guest_tools_fingerprint(root, "nested"), first)

            # Different bytes must be a change.
            compiler.write_bytes(b"second")
            self.assertNotEqual(
                product_state.guest_tools_fingerprint(root, "nested"), first)
            compiler.write_bytes(b"first")

            # A different payload name is a different tree.
            self.assertNotEqual(
                product_state.guest_tools_fingerprint(root, "tcc"), first)

            # A symlink that moves must be visible: the payload's size depends
            # on where its symlinks point, because they deduplicate libc.a and
            # the header tree.
            link = payload / "usr" / "include"
            link.symlink_to("lib/tcc/include")
            with_link = product_state.guest_tools_fingerprint(root, "nested")
            link.unlink()
            link.symlink_to("lib/tcc/other")
            self.assertNotEqual(
                product_state.guest_tools_fingerprint(root, "nested"), with_link)

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
            # The stamp records the whole image identity -- repository inputs
            # plus the staged payload -- so it is written the same way the
            # build writes it, through the one function both sides share.
            product.rootfs_stamp_path(artifacts).write_text(
                product.rootfs_image_fingerprint(artifacts, "none")
            )
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
                # The stamp records the whole image identity -- repository
                # inputs plus the staged payload -- so it is written the same
                # way the build writes it, through the shared function.
                product.rootfs_stamp_path(artifacts).write_text(
                    product.rootfs_image_fingerprint(artifacts, "none")
                )
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

    def test_rootfs_includes_locked_root_identity_for_busybox_name_lookup(self) -> None:
        script = (REPO_ROOT / "scripts/build-rootfs.sh").read_text(encoding="utf-8")
        self.assertIn(
            """cat > "$STAGE/etc/passwd" <<'EOF'\nroot:!:0:0:root:/root:/bin/sh\nEOF""",
            script,
        )
        self.assertIn(
            """cat > "$STAGE/etc/group" <<'EOF'\nroot:!:0:\nEOF""",
            script,
        )
        self.assertIn('"$STAGE/root"', script)
        self.assertIn('chmod 0644 "$STAGE/etc/passwd" "$STAGE/etc/group"', script)

    def test_rootfs_inputs_changing_during_build_do_not_publish_success_stamp(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            # The repository half changes while its own image is being built, so
            # the image is a mixture of two revisions and no stamp describes it.
            with patch.object(product, "rootfs_fingerprint",
                              side_effect=["before", "after"]), \
                    patch.object(product, "guest_tools_fingerprint",
                                 return_value="payload"), \
                    patch.object(product, "run_checked"):
                with self.assertRaisesRegex(product.ProductError, "changed during compilation"):
                    product.build_rootfs(artifacts)
            self.assertFalse(product.rootfs_stamp_path(artifacts).exists())

    def test_rebuilt_payload_does_not_stop_the_stamp_being_written(self) -> None:
        product = load_product()
        with test_tmpdir() as directory:
            artifacts = product.Artifacts(Path(directory), product.Variant("1G"))
            # The payload is rebuilt during the build, on purpose, and its bytes
            # are not reproducible -- tcc's own archive differs between two
            # builds of identical inputs.  That must not be read as the inputs
            # changing underneath the build, or no tcc image could ever be
            # published.  The stamp has to describe the payload that was
            # actually embedded, which is the post-build read.
            with patch.object(product, "rootfs_fingerprint",
                              side_effect=["repo", "repo"]), \
                    patch.object(product, "guest_tools_fingerprint",
                                 return_value="payload"), \
                    patch.object(product, "run_checked"):
                product.build_rootfs(artifacts)
            stamp = product.rootfs_stamp_path(artifacts)
            self.assertTrue(stamp.exists())
            self.assertEqual(stamp.read_text().strip(), "repo:payload")

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
            {"name": "tk-axtask", "metadata": {"thekernel": {"layer": "platform",
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
        self.assertTrue(any("tk-axtask" in command and "--features" in command for command in invocations))

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

    def test_failed_marker_stop_does_not_claim_the_marker_was_missing(self) -> None:
        from io import StringIO
        from types import SimpleNamespace

        product = load_product()
        with test_tmpdir() as directory:
            root = Path(directory)
            artifacts = product.Artifacts(root / "state", product.Variant(memory="1G"), "system")
            for path in (artifacts.kernel, artifacts.esp, artifacts.rootfs):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            result = SimpleNamespace(
                returncode=4, error_message="QMP control failed: Broken pipe",
                log_path=root / "console.log", diagnostic_log_path=root / "kernel.log",
                intentionally_stopped=False,
            )
            result.log_path.write_text(product.FBCON_MARKER + "\n", encoding="utf-8")
            stderr = StringIO()
            with patch.object(product, "run", return_value=result), patch("sys.stderr", stderr):
                code = product.run_product.__wrapped__(artifacts, product.RunSpec(
                    accel="kvm", timeout=30, workdir=root / "run", interactive=False,
                    input_after_marker=None, stop_after_marker=product.FBCON_MARKER,
                    commands=None, extra_block=None, run_cpus=4,
                ))
            self.assertEqual(code, 4)
            self.assertIn(result.error_message, stderr.getvalue())
            self.assertIn("marker-gated stop/acceptance did not complete", stderr.getvalue())
            self.assertIn(product.FBCON_MARKER, stderr.getvalue())
            self.assertNotIn("without completion marker", stderr.getvalue())

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


class DesktopHomeTests(unittest.TestCase):
    def test_run_wires_usb_input_and_storage(self):
        product = load_product()
        args = product.build_parser().parse_args([
            "run", "--no-build", "--input-backend", "usb", "--usb-disk", "/home/example/usb.img",
        ])
        with patch.object(product, "run_product", return_value=0) as run:
            self.assertEqual(product.run_cmd(args), 0)
            spec = run.call_args.args[1]
            self.assertEqual(spec.input_backend, "usb")
            self.assertEqual(spec.usb_disk, Path("/home/example/usb.img"))

    def test_desktop_defaults_to_accelerated_virgl(self):
        product = load_product()
        args = product.build_parser().parse_args(["run-gui"])
        self.assertEqual(args.graphics_profile, "virgl-interactive")
        self.assertEqual((args.width, args.height, args.accel, args.audio_backend), (1920, 1080, "kvm", "pa"))
        self.assertEqual(args.memory, "2G")
        self.assertEqual(product.build_parser().parse_args(["run-gui", "--memory", "4G"]).memory, "4G")
        self.assertEqual(product.build_parser().parse_args(["run"]).memory, "1G")

    def test_display_and_audio_arguments_reach_no_build_run(self):
        product = load_product()
        for command, extra, expected in (
            ("run", [], (800, 600, "tcg", None)),
            ("run-gui", [], (1920, 1080, "kvm", "pa")),
            ("run-gui", ["--width", "1280", "--height", "720", "--audio-backend", "wav"],
                (1280, 720, "kvm", "wav")),
        ):
            with self.subTest(command=command, extra=extra):
                args = product.build_parser().parse_args([command, "--no-build", *extra])
                with patch.object(product, "run_product", return_value=0) as run:
                    self.assertEqual(product.run_cmd(args), 0)
                spec = run.call_args.args[1]
                self.assertEqual((spec.graphics_width, spec.graphics_height, spec.accel, spec.audio_backend), expected)

    def test_invalid_display_dimensions_fail_before_build(self):
        product = load_product()
        args = product.build_parser().parse_args(["run-gui", "--width", "0"])
        with patch.object(product, "build_kernel") as build:
            with self.assertRaisesRegex(product.ProductError, "must be positive"):
                product.run_cmd(args)
            build.assert_not_called()

    def test_new_home_disk_is_ext4_and_reused_without_reformatting(self):
        product = load_product()
        if product.shutil.which("mkfs.ext4") is None:
            self.skipTest("mkfs.ext4 unavailable")
        with test_tmpdir() as directory:
            disk = Path(directory) / "home.ext4"
            self.assertEqual(product.prepare_desktop_home(disk), disk)
            self.assertEqual(disk.stat().st_size, 1024 * 1024 * 1024)
            with disk.open("rb") as image:
                image.seek(1024 + 56)
                self.assertEqual(image.read(2), b"\x53\xef")
            initial_inode = disk.stat().st_ino
            with patch.object(product, "run_checked") as format_disk:
                self.assertEqual(product.prepare_desktop_home(disk), disk)
                format_disk.assert_not_called()
            self.assertEqual(disk.stat().st_ino, initial_inode)

    def test_home_creation_failure_leaves_no_partial_disk(self):
        product = load_product()
        with test_tmpdir() as directory:
            disk = Path(directory) / "home.ext4"
            with patch.object(product.shutil, "which", return_value="mkfs.ext4"), \
                    patch.object(product, "run_checked", side_effect=product.ProductError("format failed")):
                with self.assertRaisesRegex(product.ProductError, "format failed"):
                    product.prepare_desktop_home(disk)
            self.assertFalse(disk.exists())
            self.assertEqual(list(disk.parent.glob(".home-*")), [])

    def test_home_creation_never_overwrites_external_creator(self):
        product = load_product()
        with test_tmpdir() as directory:
            disk = Path(directory) / "home.ext4"
            with patch.object(product.shutil, "which", return_value="mkfs.ext4"), \
                    patch.object(product, "run_checked", side_effect=lambda _: disk.write_bytes(b"existing data")):
                product.prepare_desktop_home(disk)
            self.assertEqual(disk.read_bytes(), b"existing data")

    def test_run_gui_wires_persistent_home_as_extra_disk(self):
        product = load_product()
        args = product.build_parser().parse_args([
            "run-gui", "--no-build", "--rootfs", "/home/example/rootfs.ext2",
            "--home-disk", "/home/example/home.ext4",
        ])
        with patch.object(product, "prepare_desktop_home", return_value=Path(args.home_disk)) as prepare, \
                patch.object(product, "run_cmd", return_value=0) as run:
            self.assertEqual(product.run_gui_cmd(args), 0)
            prepare.assert_called_once_with(Path("/home/example/home.ext4"))
            self.assertEqual(run.call_args.args[0].extra_block, "/home/example/home.ext4")
            self.assertEqual(run.call_args.args[0].rootfs_transport, "drive")

    def test_missing_default_desktop_explains_rebuild_before_touching_home(self):
        product = load_product()
        with test_tmpdir() as directory:
            args = product.build_parser().parse_args(["run-gui"])
            with patch.object(product, "state_root", return_value=Path(directory)), \
                    patch.object(product, "prepare_desktop_home") as prepare, \
                    patch.object(product, "build_desktop_rootfs") as build, \
                    patch.object(product, "run_cmd") as run:
                with self.assertRaisesRegex(product.ProductError, "make run-gui RUN_ARGS=--build"):
                    product.run_gui_cmd(args)
                prepare.assert_not_called()
                build.assert_not_called()
                run.assert_not_called()

    def test_run_gui_rejects_conflicting_extra_disk(self):
        product = load_product()
        args = product.build_parser().parse_args(["run-gui", "--extra-block", "/home/example/other.ext4"])
        with patch.object(product, "prepare_desktop_home") as prepare:
            with self.assertRaisesRegex(product.ProductError, "use --home-disk"):
                product.run_gui_cmd(args)
            prepare.assert_not_called()

    def test_default_home_disk_uses_durable_xdg_data_directory(self):
        product = load_product()
        with test_tmpdir() as directory:
            disk = Path(directory) / "thekernel/desktop/home.ext4"
            disk.parent.mkdir(parents=True)
            disk.write_bytes(b"existing data")
            with patch.dict(os.environ, {"XDG_DATA_HOME": directory}):
                self.assertEqual(product.prepare_desktop_home(), disk)


if __name__ == "__main__":
    unittest.main()
