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
        return f"mem{self.memory.lower()}{suffix}"


@dataclass(frozen=True)
class Artifacts:
    root: Path
    variant: Variant
    profile: str = "system"

    @property
    def output_dir(self) -> Path:
        return self.root / "out" / "x86_64" / "q35-uefi" / self.profile / self.variant.name

    @property
    def cargo_target_dir(self) -> Path:
        return self.root / "target" / "thekernel" / "x86_64" / "q35-uefi" / self.profile / self.variant.name

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
        return self.root / "out" / "rootfs" / "x86" / "rootfs-x86.img"


def artifact_config_stamp(artifacts: Artifacts, transport: str) -> Path:
    return artifacts.esp_for_rootfs_transport(transport).with_suffix(".config-stamp")


def artifact_input_key(artifacts: Artifacts, rootfs: Path | None, transport: str) -> str:
    digest = hashlib.sha256()
    grub = "grub.cfg" if transport == "module" else "grub-drive.cfg"
    for relative in ("config/kernel.toml", "config/x86_64/q35-uefi.toml", "rust-toolchain.toml",
                     "scripts/build-x86-uefi-esp.sh", f"config/x86_64/{grub}"):
        content = (REPO_ROOT / relative).read_bytes()
        digest.update(len(content).to_bytes(8, "little"))
        digest.update(content)
    image = (rootfs or artifacts.rootfs).resolve()
    digest.update(repr((artifacts.variant, artifacts.profile, transport, str(image))).encode())
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
            current_tests = rootfs_stamp_path(artifacts).read_text().strip() == rootfs_fingerprint()
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
)


def rootfs_stamp_path(artifacts: Artifacts) -> Path:
    return artifacts.rootfs.with_name(artifacts.rootfs.name + ".stamp")


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
