"""Product artifact layout, disk storage policy, locking and cache validity."""

from __future__ import annotations

import fcntl
import glob as glob_module
import hashlib
import os
import re
from contextlib import contextmanager, ExitStack
from dataclasses import dataclass
from functools import wraps
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
TARGET = "x86_64-unknown-none"
PLATFORM = "x86-pc"
MEMORY_RE = re.compile(r"([1-9][0-9]*)([KMG])", re.IGNORECASE)


class ProductError(RuntimeError):
    """Raised for an invalid or failed product operation."""


def state_root() -> Path:
    configured = os.environ.get("THEKERNEL_STATE_DIR", "").strip()
    # Product artifacts are intentionally outside the checkout.  Apart from
    # keeping the tree clean, this keeps all large, regenerable targets on the
    # host filesystem rather than a transient mount.
    state = (
        Path(configured).expanduser()
        if configured
        else Path.home() / ".cache" / "thekernel-targets"
    )
    if not state.is_absolute():
        state = REPO_ROOT / state
    state = state.resolve()
    validate_storage(state)
    return state


def validate_storage(path: Path) -> None:
    resolved = path.expanduser().resolve()
    mounts = []
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        fields = line.split()
        mount = Path(fields[4].replace(r"\040", " "))
        if resolved == mount or mount in resolved.parents:
            mounts.append((len(mount.parts), fields[fields.index("-") + 1]))
    tmpfs = bool(mounts and max(mounts)[1] in {"tmpfs", "ramfs"})
    if tmpfs or any(resolved == base or base in resolved.parents for base in (Path("/tmp"), Path("/dev/shm"))):
        raise ProductError(f"artifacts must be stored on disk, outside tmpfs: {resolved}")


@contextmanager
def state_lock(name: str, *, shared: bool = False, blocking: bool = True, root: Path | None = None):
    directory = (root if root is not None else state_root()) / "locks"
    directory.mkdir(parents=True, exist_ok=True)
    with (directory / f"{name}.lock").open("a+") as handle:
        operation = fcntl.LOCK_SH if shared else fcntl.LOCK_EX
        try:
            fcntl.flock(handle, operation | (0 if blocking else fcntl.LOCK_NB))
        except BlockingIOError as error:
            raise ProductError(f"state lock {name} is active; wait for its current operation to finish") from error
        try:
            yield
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)


def serialized_build(function):
    @wraps(function)
    def locked(*args, **kwargs):
        with state_lock("build", root=args[0].root):
            return function(*args, **kwargs)
    return locked


def isolated_run(function):
    @wraps(function)
    def locked(artifacts, spec):
        with ExitStack() as stack:
            stack.enter_context(state_lock("build", shared=True, root=artifacts.root))
            if spec.workdir is not None:
                path = str(spec.workdir.expanduser().resolve())
                key = hashlib.sha256(path.encode()).hexdigest()[:24]
                stack.enter_context(state_lock(f"run-{key}", blocking=False, root=artifacts.root))
            validate_artifact_config(artifacts, spec.rootfs, spec.rootfs_transport)
            return function(artifacts, spec)
    return locked


@dataclass(frozen=True)
class Variant:
    memory: str
    asid_fast_switch: bool = False
    m5_candidate: bool = False
    io_submit_batch: bool = False
    io_notify_fastpath: bool = False
    # Builds the Intel i225/i226 (igc) driver into the product kernel.  It is a
    # separate variant because the product's static NIC type is `virtio-net`
    # and one non-`dyn` build cannot be both; the flag exists so the driver can
    # be built, booted on a machine that has no such part, and seen to reject
    # it -- the only automated run this workstream has.
    net_igc: bool = False

    @property
    def memory_bytes(self) -> int:
        match = MEMORY_RE.fullmatch(self.memory)
        assert match is not None
        value = int(match.group(1))
        shift = {"K": 10, "M": 20, "G": 30}[match.group(2).upper()]
        return value << shift

    @property
    def name(self) -> str:
        suffix = "-asid-fast-switch" if self.asid_fast_switch else ""
        if self.m5_candidate:
            suffix += "-m5-candidate"
        if self.io_submit_batch:
            suffix += "-io-submit-batch"
        if self.io_notify_fastpath:
            suffix += "-io-notify-fastpath"
        if self.net_igc:
            suffix += "-net-igc"
        return f"mem{self.memory.lower()}{suffix}"


@dataclass(frozen=True)
class MachineProfile:
    """One machine profile a product image can be built for.

    A profile fixes the machine facts the kernel needs at compile time: which
    configuration file to generate from, how many per-CPU slots the image must
    preallocate, and how many QEMU CPUs a run may request.  Everything the
    firmware can answer instead (installed RAM, the CPUs that actually exist,
    the PCI ECAM base) is deliberately *not* here.
    """

    name: str
    config: str
    max_cpus: int


Q35_UEFI_PROFILE = MachineProfile(
    name="q35-uefi",
    config="config/x86_64/q35-uefi.toml",
    # One product ELF has four preallocated slots; QEMU's `-smp` chooses how
    # many of them come online for an UP or SMP4 run.
    max_cpus=4,
)

# A real machine needs more slots than a QEMU smoke test: the i3-N305 has eight
# E-cores and no SMT, so a four-slot image can never bring the whole machine up.
N305_PROFILE = MachineProfile(
    name="n305",
    config="config/x86_64/n305.toml",
    max_cpus=8,
)

MACHINE_PROFILES = {profile.name: profile for profile in (Q35_UEFI_PROFILE, N305_PROFILE)}


def machine_profile(name: str) -> MachineProfile:
    try:
        return MACHINE_PROFILES[name]
    except KeyError:
        raise ProductError(
            f"unknown machine profile {name!r}; expected one of "
            f"{', '.join(sorted(MACHINE_PROFILES))}"
        ) from None


@dataclass(frozen=True)
class Artifacts:
    root: Path
    variant: Variant
    profile: str = "system"
    machine: MachineProfile = Q35_UEFI_PROFILE

    @property
    def output_dir(self) -> Path:
        return self.root / "out" / "x86_64" / self.machine.name / self.profile / self.variant.name

    @property
    def cargo_target_dir(self) -> Path:
        return (self.root / "target" / "thekernel" / "x86_64" / self.machine.name
                / self.profile / self.variant.name)

    @property
    def config_path(self) -> Path:
        return self.cargo_target_dir / "config" / "axconfig.toml"

    @property
    def linker_script(self) -> Path:
        return self.cargo_target_dir / TARGET / "release" / f"linker_{PLATFORM}.lds"

    @property
    def cargo_elf(self) -> Path:
        return self.cargo_target_dir / TARGET / "release" / "thekernel"

    @property
    def kernel(self) -> Path:
        return self.output_dir / "kernel-x86_64"

    @property
    def esp(self) -> Path:
        return self.output_dir / "kernel-x86_64.esp"

    @property
    def drive_esp(self) -> Path:
        """UEFI ESP for a rootfs supplied exclusively as virtio-blk."""

        return self.output_dir / "kernel-x86_64-drive.esp"

    def esp_for_rootfs_transport(self, rootfs_transport: str) -> Path:
        if rootfs_transport == "module":
            return self.esp
        if rootfs_transport == "drive":
            return self.drive_esp
        raise ProductError(f"unsupported product rootfs transport: {rootfs_transport}")

    @property
    def rootfs(self) -> Path:
        payload = selected_tool_payload()
        # A tool payload changes what the image contains, so it must not share
        # the baseline image: the kernel embeds this file, and a payload image
        # replacing the default one would silently change an unrelated suite.
        name = "rootfs-x86.img" if payload == "none" else f"rootfs-x86-{payload}.img"
        return self.root / "out" / "rootfs" / "x86" / name


def artifact_config_stamp(artifacts: Artifacts, transport: str) -> Path:
    return artifacts.esp_for_rootfs_transport(transport).with_suffix(".config-stamp")


def artifact_input_key(artifacts: Artifacts, rootfs: Path | None, transport: str) -> str:
    digest = hashlib.sha256()
    grub = "grub.cfg" if transport == "module" else "grub-drive.cfg"
    for relative in ("config/kernel.toml", artifacts.machine.config, "rust-toolchain.toml",
                     "scripts/build-x86-uefi-esp.sh", f"config/x86_64/{grub}"):
        content = (REPO_ROOT / relative).read_bytes()
        digest.update(len(content).to_bytes(8, "little"))
        digest.update(content)
    image = (rootfs or artifacts.rootfs).resolve()
    digest.update(repr((artifacts.variant, artifacts.profile, artifacts.machine.name, transport,
                        str(image))).encode())
    with image.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    settings = (os.environ.get("AX_LOG") or "info", os.environ.get("AX_BACKTRACE") or "n",
                os.environ.get("RUSTFLAGS", "").strip())
    digest.update(repr(settings).encode())
    return digest.hexdigest()


def artifact_config_key(artifacts: Artifacts, rootfs: Path | None, transport: str) -> str:
    digest = hashlib.sha256(artifact_input_key(artifacts, rootfs, transport).encode())
    # The standalone kernel is shared by module/drive boot variants. A rebuild
    # of one must invalidate the other's older embedded kernel, even if its
    # configuration and rootfs did not change.
    for path in (artifacts.kernel, artifacts.esp_for_rootfs_transport(transport)):
        digest.update(path.stat().st_size.to_bytes(8, "little"))
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
    return digest.hexdigest()


def validate_artifact_config(artifacts: Artifacts, rootfs: Path | None, transport: str) -> None:
    image = (rootfs or artifacts.rootfs).resolve()
    if image == artifacts.rootfs.resolve():
        try:
            current_tests = (
                rootfs_stamp_path(artifacts).read_text().strip()
                == rootfs_image_fingerprint(artifacts, selected_tool_payload())
            )
        except OSError:
            current_tests = False
        if not current_tests:
            raise ProductError("guest test sources or rootfs build inputs changed; rebuild before running")
    stamp = artifact_config_stamp(artifacts, transport)
    try:
        matches = stamp.read_text().strip() == artifact_config_key(artifacts, rootfs, transport)
    except OSError:
        matches = False
    if not matches:
        raise ProductError("artifact configuration or rootfs changed; rebuild before running")


# Inputs that change the published rootfs image.  The BusyBox version and
# download URL live in build-rootfs.sh itself, so hashing the script covers
# them.
ROOTFS_INPUT_FILES = (
    "scripts/build-rootfs.sh",
    "scripts/build-guest-tools.sh",
    "scripts/create-rootfs-image.sh",
    "tests/guest/shell-init.sh",
    "tests/guest/system-init.c",
)
ROOTFS_INPUT_GLOBS = (
    "tests/rootfs/busybox-*.config",
    "tests/guest/tools/*.c",
    "tests/guest/portable/*.c",
)
# Environment switches that change the toolchain or image ownership.
ROOTFS_INPUT_ENV = (
    "THEKERNEL_X86_CROSS_COMPILE",
    "THEKERNEL_USE_LOCAL_MUSL",
    "THEKERNEL_MUSL_ROOT",
    "THEKERNEL_MUSL_LINUX_UAPI_INCLUDE",
    "THEKERNEL_MUSL_LINUX_ARCH_INCLUDE",
    "THEKERNEL_ROOTFS_OWNER_MODE",
    "THEKERNEL_TOOLCHAIN",
)

# The optional guest tool payload selected by --toolchain.  `none` keeps the
# baseline image and is the only selection the ordinary suites use.  A tool
# payload adds executables and data to the image, which is tens of MiB: `tcc`
# adds a native C compiler and its musl sysroot, and `nested` is a superset of
# it that also adds a static system emulator and the image it boots.  Each
# selection gets its own image, because the kernel embeds it and the two
# payloads must never be confused for one another.
TOOL_PAYLOADS = ("none", "tcc", "nested", "glibc")


def selected_tool_payload(requested: str | None = None) -> str:
    """The guest tool payload this process is building for.

    `requested` is the parsed `--toolchain` value; it takes precedence over
    THEKERNEL_TOOLCHAIN so the flag is never silently discarded by an exported
    environment variable.  Callers without the flag pass nothing and inherit
    the environment.
    """

    payload = (requested or os.environ.get("THEKERNEL_TOOLCHAIN", "")).strip() or "none"
    if payload not in TOOL_PAYLOADS:
        raise ProductError(
            f"unknown guest tool payload {payload!r}; expected one of "
            f"{', '.join(TOOL_PAYLOADS)}"
        )
    return payload


def rootfs_image_bytes(payload: str) -> int:
    """Image size for a payload.

    These are allocations, not measurements: the baseline image is the
    historical 96 MiB, and each payload's size was chosen from what it
    actually stages with headroom for the build tree that lands beside it.
    """

    # `glibc` stages a loader, a shared libc and one dynamic binary: about
    # 3.5 MiB of content, so it needs no more room than the baseline.
    return {"none": 96, "tcc": 160, "nested": 224, "glibc": 160}[payload] * 1024 * 1024



def rootfs_stamp_path(artifacts: Artifacts) -> Path:
    return artifacts.rootfs.with_name(artifacts.rootfs.name + ".stamp")


def rootfs_image_fingerprint(artifacts: Artifacts, payload: str) -> str:
    """The identity of the image a rootfs build would produce.

    Two things go into the image and therefore into its identity: the
    repository-side build inputs, and the staged tool payload the image embeds.
    They are combined here, in one place, because the writer and the reader of
    the stamp must agree exactly.  They did not: the build wrote the pair while
    the validator compared the repository half alone, so every run after a
    successful build reported "guest test sources or rootfs build inputs
    changed; rebuild before running" and refused to test an image it had just
    built correctly.
    """

    return f"{rootfs_fingerprint()}:{guest_tools_fingerprint(artifacts.root, payload)}"


def guest_tools_dir(root, payload: str):
    """Where build-guest-tools.sh stages `payload` under the state root."""

    return Path(root) / "guest-tools" / payload


def guest_tools_fingerprint(root, payload: str) -> str:
    """Identify the staged guest tool payload a rootfs image would be built from.

    The rootfs image embeds this tree, so a change in it has to change the
    image.  Without this, rebuilding the payload and rebuilding the image were
    independent events: an image built from a payload that lacked the compiler
    was reused for a payload that had it, and the extra case the image's own
    plan promised then failed inside the guest.  That failure is reported by
    the suite as a compiler that is not there, which points at the compiler
    rather than at a stale image.

    Content is hashed, not timestamps.  The payload is rebuilt on every build,
    so every file it contains gets a fresh modification time even when its
    bytes are identical -- which is the normal case.  A timestamp-based
    fingerprint therefore reports a change on every single run, and a check
    that compares the payload before and after a rebuild can never agree with
    itself.  Content is what decides whether the embedded image differs, so
    content is what is hashed.  It costs a few hundred milliseconds over the
    payload's hundred MiB, and only when a tool payload is selected at all.
    """

    directory = guest_tools_dir(root, payload)
    if not directory.is_dir():
        return "absent"
    digest = hashlib.sha256()
    entries = sorted(
        path for path in directory.rglob("*") if path.is_file() or path.is_symlink()
    )
    for path in entries:
        relative = path.relative_to(directory).as_posix().encode()
        digest.update(len(relative).to_bytes(8, "little"))
        digest.update(relative)
        if path.is_symlink():
            # A symlink's own content is its target; the payload uses them to
            # deduplicate libc.a and the header tree, so the target matters.
            digest.update(b"->" + os.readlink(path).encode())
        elif path.is_file():
            content = path.read_bytes()
            digest.update(len(content).to_bytes(8, "little"))
            digest.update(content)
    return digest.hexdigest()


def rootfs_fingerprint() -> str:
    digest = hashlib.sha256()
    inputs = [REPO_ROOT / relative for relative in ROOTFS_INPUT_FILES]
    for pattern in ROOTFS_INPUT_GLOBS:
        inputs.extend(
            Path(path) for path in sorted(glob_module.glob(str(REPO_ROOT / pattern)))
        )
    for path in inputs:
        relative = path.relative_to(REPO_ROOT).as_posix().encode()
        content = path.read_bytes()
        digest.update(len(relative).to_bytes(8, "little"))
        digest.update(relative)
        digest.update(len(content).to_bytes(8, "little"))
        digest.update(content)
    for name in ROOTFS_INPUT_ENV:
        digest.update(f"{name}={os.environ.get(name, '')}".encode())
    return digest.hexdigest()
