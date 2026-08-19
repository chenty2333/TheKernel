#!/usr/bin/env python3
"""Content-addressed builder for TheKernel kernel artifacts."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
import tomllib
from collections.abc import Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Literal

REPO_FOR_IMPORTS = Path(__file__).resolve().parents[1]
if str(REPO_FOR_IMPORTS) not in sys.path:
    sys.path.insert(0, str(REPO_FOR_IMPORTS))

from tools.project_paths import repo_root as find_repo_root  # noqa: E402


CACHE_VERSION = 4
LOCK_STALE_SECS = 30 * 60
LOCK_POLL_SECS = 0.2
IGNORE_DIR_NAMES = frozenset({"__pycache__", ".git"})
IGNORE_FILE_SUFFIXES = (".pyc",)

KERNEL_ENV: Mapping[str, str] = {
    "DEBUGINFO": "y",
    "DWARF": "n",
    "LOG": "warn",
    "BANNER": "n",
    "BACKTRACE": "n",
    "NO_AXSTD": "y",
    "AX_LIB": "axfeat",
    "BLK": "y",
    "NET": "y",
    "VSOCK": "n",
    "MEM": "1G",
    "LTO": "",
    "MODE": "release",
}

class BuildError(RuntimeError):
    """Raised when an artifact cannot be produced."""


@dataclass(frozen=True)
class InputSpec:
    kind: Literal["file", "tree", "optional_file", "optional_tree"]
    path: str


@dataclass(frozen=True)
class BuildResult:
    kind: str
    name: str
    cache_path: Path
    output_path: Path
    identity: str
    hit: bool


@dataclass(frozen=True)
class KernelRequest:
    name: str
    arch: Literal["x86_64"]
    make_args: tuple[str, ...]
    app_features: str
    root: Path


@dataclass(frozen=True)
class RootfsRequest:
    arch: Literal["x86"]
    root: Path


class FileDigestStore:
    """Cache sha256(file) by realpath, size, and mtime."""

    def __init__(self, db_path: Path) -> None:
        self.db_path = db_path
        self._local = threading.local()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_db()

    def _connect(self) -> sqlite3.Connection:
        conn = getattr(self._local, "conn", None)
        if conn is None:
            conn = sqlite3.connect(self.db_path, timeout=60)
            conn.execute("PRAGMA journal_mode=WAL")
            conn.execute("PRAGMA synchronous=NORMAL")
            self._local.conn = conn
        return conn

    def _init_db(self) -> None:
        conn = self._connect()
        conn.execute(
            """
            CREATE TABLE IF NOT EXISTS file_digests (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                sha256 TEXT NOT NULL
            )
            """
        )
        conn.commit()

    def digest_file(self, path: Path) -> str:
        path = path.resolve()
        st = path.stat()
        size = int(st.st_size)
        mtime_ns = int(getattr(st, "st_mtime_ns", int(st.st_mtime * 1_000_000_000)))
        conn = self._connect()
        row = conn.execute(
            "SELECT size, mtime_ns, sha256 FROM file_digests WHERE path = ?",
            (str(path),),
        ).fetchone()
        if row is not None and int(row[0]) == size and int(row[1]) == mtime_ns:
            return str(row[2])

        sha = sha256_file(path)
        conn.execute("BEGIN IMMEDIATE")
        try:
            conn.execute(
                """
                INSERT INTO file_digests(path, size, mtime_ns, sha256)
                VALUES (?, ?, ?, ?)
                ON CONFLICT(path) DO UPDATE SET
                    size = excluded.size,
                    mtime_ns = excluded.mtime_ns,
                    sha256 = excluded.sha256
                """,
                (str(path), size, mtime_ns, sha),
            )
            conn.commit()
        except Exception:
            conn.rollback()
            raise
        return sha

    def close(self) -> None:
        conn = getattr(self._local, "conn", None)
        if conn is not None:
            conn.close()
            self._local.conn = None


def state_root(root: Path | None = None) -> Path:
    """Return the resolved state root used by all generated build artifacts.

    ``THEKERNEL_STATE_DIR`` is the process-level override used by the shell
    gates and Makefile.  Relative overrides are rooted at the repository, so
    callers do not accidentally interpret them relative to an unrelated
    working directory.
    """

    repo = (root or find_repo_root()).expanduser().resolve()
    configured = os.environ.get("THEKERNEL_STATE_DIR", "").strip()
    state = Path(configured).expanduser() if configured else repo / ".state"
    if not state.is_absolute():
        state = repo / state
    return state.resolve()


def state_path(
    root: Path | None = None, *parts: str | os.PathLike[str]
) -> Path:
    """Resolve a path below the configured state root.

    Build paths are fixed subdirectories, but resolving and checking them here
    also prevents an existing symlink component from redirecting generated
    state outside the selected root.
    """

    state = state_root(root)
    candidate = state.joinpath(*parts).resolve()
    try:
        candidate.relative_to(state)
    except ValueError as error:
        raise BuildError(
            f"state path escapes configured state directory: {candidate}"
        ) from error
    return candidate


def build_cache_root(root: Path | None = None) -> Path:
    return state_path(root, "build-cache")


def digests_db_path(root: Path | None = None) -> Path:
    return build_cache_root(root) / "file-digests.sqlite"


def locks_dir(root: Path | None = None) -> Path:
    return build_cache_root(root) / "locks"


def kernel_cache_dir(root: Path | None = None) -> Path:
    return build_cache_root(root) / "kernels"


def rootfs_cache_dir(root: Path | None = None) -> Path:
    return build_cache_root(root) / "rootfs"


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def canonical_params_json(params: Mapping[str, str]) -> str:
    normalized = {str(key): str(value) for key, value in params.items()}
    return json.dumps(normalized, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def hash_params(params: Mapping[str, str]) -> str:
    return sha256_text(canonical_params_json(params))


def fingerprint_inputs(
    specs: Sequence[InputSpec],
    *,
    root: Path,
    digests: FileDigestStore,
) -> str:
    entries: list[tuple[str, str]] = []
    root = root.resolve()
    for spec in specs:
        abs_path = (root / spec.path).resolve() if not os.path.isabs(spec.path) else Path(spec.path).resolve()
        if spec.kind in ("file", "optional_file"):
            if not abs_path.is_file():
                if spec.kind == "optional_file":
                    entries.append((f"absent:{relpath(root, abs_path, spec.path)}", "ABSENT"))
                    continue
                raise FileNotFoundError(f"required input file missing: {spec.path}")
            entries.append((relpath(root, abs_path, spec.path), digests.digest_file(abs_path)))
            continue

        if spec.kind in ("tree", "optional_tree"):
            if not abs_path.is_dir():
                if spec.kind == "optional_tree":
                    entries.append((f"absent:{relpath(root, abs_path, spec.path)}", "ABSENT"))
                    continue
                raise FileNotFoundError(f"required input tree missing: {spec.path}")
            tree_key = relpath(root, abs_path, spec.path).rstrip("/")
            for file_path in iter_tree_files(abs_path):
                relative_file = file_path.relative_to(abs_path).as_posix()
                entries.append(
                    (f"{tree_key}/{relative_file}", digests.digest_file(file_path))
                )
            continue
        raise ValueError(f"unknown input kind: {spec.kind}")

    entries.sort(key=lambda item: item[0])
    digest = hashlib.sha256()
    for key, value in entries:
        digest.update(key.encode())
        digest.update(b"\0")
        digest.update(value.encode())
        digest.update(b"\0")
    return digest.hexdigest()


def iter_tree_files(directory: Path) -> Iterable[Path]:
    for dirpath, dirnames, filenames in os.walk(directory, followlinks=False):
        dirnames[:] = sorted(name for name in dirnames if name not in IGNORE_DIR_NAMES)
        for name in sorted(filenames):
            if name.endswith(IGNORE_FILE_SUFFIXES):
                continue
            path = Path(dirpath) / name
            if path.is_file():
                yield path


def relpath(root: Path, path: Path, fallback: str) -> str:
    try:
        return path.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return fallback.replace("\\", "/")


def safe_name(text: str) -> str:
    allowed = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")
    return "".join(ch if ch in allowed else "_" for ch in text)


def identity_for(
    *,
    kind: str,
    params: Mapping[str, str],
    inputs: str,
) -> str:
    params_hex = hash_params(params)
    return sha256_text(f"{kind}\0{CACHE_VERSION}\0{params_hex}\0{inputs}")


def cache_file_is_ready(path: Path) -> bool:
    try:
        return path.is_file() and path.stat().st_size > 0
    except OSError:
        return False


def lock_path(root: Path, kind: str, name: str, identity: str) -> Path:
    return locks_dir(root) / f"{safe_name(kind)}-{safe_name(name)}-{identity[:16]}.lock"


@contextlib.contextmanager
def exclusive_identity_lock(root: Path, kind: str, name: str, identity: str) -> Iterator[None]:
    path = lock_path(root, kind, name, identity)
    path.parent.mkdir(parents=True, exist_ok=True)
    while True:
        try:
            path.mkdir()
            break
        except FileExistsError:
            if stale_lock(path):
                shutil.rmtree(path, ignore_errors=True)
                continue
            time.sleep(LOCK_POLL_SECS)
    try:
        yield
    finally:
        shutil.rmtree(path, ignore_errors=True)


def stale_lock(path: Path) -> bool:
    try:
        age = time.time() - path.stat().st_mtime
    except OSError:
        return False
    return age > LOCK_STALE_SECS


def ensure_cached_artifact(
    *,
    kind: str,
    name: str,
    params: Mapping[str, str],
    input_specs: Sequence[InputSpec],
    cache_path_for_identity: Callable[[str], Path],
    build: Callable[[Path], None],
    root: Path,
    verbose: bool = False,
) -> BuildResult:
    store = FileDigestStore(digests_db_path(root))
    try:
        inputs = fingerprint_inputs(input_specs, root=root, digests=store)
        identity = identity_for(kind=kind, params=params, inputs=inputs)
        cache_path = cache_path_for_identity(identity)
        if cache_file_is_ready(cache_path):
            if verbose:
                print(f"{cache_path} is up to date")
            return BuildResult(kind, name, cache_path, cache_path, identity, True)

        with exclusive_identity_lock(root, kind, name, identity):
            inputs = fingerprint_inputs(input_specs, root=root, digests=store)
            identity = identity_for(kind=kind, params=params, inputs=inputs)
            cache_path = cache_path_for_identity(identity)
            if cache_file_is_ready(cache_path):
                if verbose:
                    print(f"{cache_path} is up to date")
                return BuildResult(kind, name, cache_path, cache_path, identity, True)

            if verbose:
                print(f"building {cache_path} ({identity[:16]})")
            cache_path.parent.mkdir(parents=True, exist_ok=True)
            tmp = cache_path.with_name(f".{cache_path.name}.tmp.{os.getpid()}")
            tmp.unlink(missing_ok=True)
            try:
                build(tmp)
                if not cache_file_is_ready(tmp):
                    raise BuildError(f"builder did not create non-empty output: {tmp}")
                tmp.replace(cache_path)
            finally:
                tmp.unlink(missing_ok=True)
            return BuildResult(kind, name, cache_path, cache_path, identity, False)
    finally:
        store.close()


def kernel_params(req: KernelRequest) -> dict[str, str]:
    params = {
        "name": req.name,
        "arch": req.arch,
        "make_args": " ".join(req.make_args),
        "app_features": " ".join(req.app_features.split()),
        "strip": "rust-objcopy --strip-all",
        "rustc": capture(["rustc", "-Vv"]),
        "cargo": capture(["cargo", "-V"]),
        "rust_objcopy": capture(["rust-objcopy", "--version"], max_lines=3),
    }
    for key, value in KERNEL_ENV.items():
        params[f"make.{key}"] = value
    return params


def _manifest_path_values(value: object) -> Iterator[str]:
    if isinstance(value, dict):
        path = value.get("path")
        if isinstance(path, str):
            yield path
        for nested in value.values():
            yield from _manifest_path_values(nested)
    elif isinstance(value, list):
        for nested in value:
            yield from _manifest_path_values(nested)


def _relative_input_path(root: Path, path: Path) -> str:
    return Path(os.path.relpath(path.resolve(), root.resolve())).as_posix()


def _external_workspace_root(package: Path, consumer_root: Path) -> Path | None:
    common = Path(os.path.commonpath((package.resolve(), consumer_root.resolve())))
    current = package.resolve()
    while current != common:
        manifest = current / "Cargo.toml"
        if manifest.is_file():
            data = tomllib.loads(manifest.read_text(encoding="utf-8"))
            if isinstance(data.get("workspace"), dict):
                return current
        current = current.parent
    return None


def external_kernel_input_specs(root: Path) -> list[InputSpec]:
    """Fingerprint the external source roots named by the live manifest.

    Normal development manifests resolve maintained crates through sibling
    workspaces, while the release-consumer gate rewrites those paths to exact
    extracted artifacts. Deriving the roots from Cargo.toml keeps both graphs
    cache-sound without requiring a source-workspace-shaped release fixture.
    """

    manifest = root / "Cargo.toml"
    data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    external_packages: dict[Path, None] = {}
    for raw_path in _manifest_path_values(data):
        package = (root / raw_path).resolve()
        if not package.is_relative_to(root.resolve()):
            external_packages.setdefault(package, None)

    specs: dict[InputSpec, None] = {}
    workspaces: dict[Path, None] = {}
    for package in external_packages:
        workspace = _external_workspace_root(package, root)
        if workspace is None:
            specs.setdefault(InputSpec("tree", _relative_input_path(root, package)), None)
            continue
        workspaces.setdefault(workspace, None)
        crates = workspace / "crates"
        if crates.is_dir() and package.is_relative_to(crates):
            specs.setdefault(InputSpec("tree", _relative_input_path(root, crates)), None)
        else:
            specs.setdefault(InputSpec("tree", _relative_input_path(root, package)), None)

    for workspace in workspaces:
        specs.setdefault(
            InputSpec("file", _relative_input_path(root, workspace / "Cargo.toml")),
            None,
        )
        for support in ("Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"):
            specs.setdefault(
                InputSpec(
                    "optional_file",
                    _relative_input_path(root, workspace / support),
                ),
                None,
            )
    return list(specs)


def kernel_input_specs(req: KernelRequest) -> list[InputSpec]:
    specs = [
        InputSpec("file", "Cargo.toml"),
        InputSpec("file", "Cargo.lock"),
        InputSpec("file", "rust-toolchain.toml"),
        InputSpec("file", "tools/build.py"),
        InputSpec("file", "kernel/Cargo.toml"),
        InputSpec("optional_file", ".cargo/config.toml"),
        InputSpec("tree", "src"),
        InputSpec("tree", "kernel/src"),
        InputSpec("tree", "crates"),
        InputSpec("tree", "third_party/rust-patches"),
        InputSpec("tree", "make"),
        # External Cargo path dependencies are build inputs, not merely
        # repositories mentioned by release tooling. Their live manifest
        # roots must be part of the content-addressed identity.
    ]
    specs.insert(5, InputSpec("file", "scripts/build-x86-uefi-esp.sh"))
    specs.insert(6, InputSpec("file", "scripts/check-x86-multiboot.sh"))
    return [*specs, *external_kernel_input_specs(req.root)]


def _parse_kernel_cpu_count(value: str, variable: str) -> int | None:
    if not value:
        return None
    if (
        not value.isascii()
        or not value.isdecimal()
        or int(value, 10) <= 0
        or int(value, 10) > 4096
    ):
        raise BuildError(f"{variable} must be an integer between 1 and 4096")
    return int(value, 10)


def requested_kernel_cpu_count() -> int | None:
    product_value = _parse_kernel_cpu_count(
        os.environ.get("THEKERNEL_KERNEL_CPUS", ""), "THEKERNEL_KERNEL_CPUS"
    )
    make_value = _parse_kernel_cpu_count(os.environ.get("SMP", ""), "SMP")
    if product_value is not None and make_value is not None:
        if product_value != make_value:
            raise BuildError(
                "THEKERNEL_KERNEL_CPUS and SMP request different CPU counts"
            )
    return product_value if product_value is not None else make_value


def _parse_kernel_memory(value: str, variable: str) -> str | None:
    if not value:
        return None
    suffix = value[-1:].upper()
    digits = value[:-1]
    if (
        suffix not in ("K", "M", "G")
        or not digits.isascii()
        or not digits.isdecimal()
        or int(digits, 10) <= 0
    ):
        raise BuildError(f"{variable} must be a positive K/M/G size")
    return f"{int(digits, 10)}{suffix}"


def requested_kernel_memory() -> str | None:
    product_value = _parse_kernel_memory(
        os.environ.get("THEKERNEL_KERNEL_MEMORY", ""),
        "THEKERNEL_KERNEL_MEMORY",
    )
    make_value = _parse_kernel_memory(os.environ.get("MEM", ""), "MEM")
    if product_value is not None and make_value is not None:
        if product_value != make_value:
            raise BuildError("THEKERNEL_KERNEL_MEMORY and MEM request different sizes")
    return product_value if product_value is not None else make_value


def requested_asid_fast_switch() -> bool:
    value = os.environ.get("THEKERNEL_KERNEL_ASID_FAST_SWITCH", "").strip().lower()
    if value in ("", "0", "false"):
        return False
    if value in ("1", "true"):
        return True
    raise BuildError(
        "THEKERNEL_KERNEL_ASID_FAST_SWITCH must be one of 0, 1, false, or true"
    )


def make_kernel_request(
    mode: Literal[
        "release", "shell", "io-test-shell", "mm-performance-shell"
    ],
    arch: str,
    root: Path,
) -> KernelRequest:
    normalize_short_arch(arch)
    full_arch: Literal["x86_64"] = "x86_64"
    name = "kernel-x86_64"
    features = "qemu"
    if mode in ("shell", "io-test-shell", "mm-performance-shell"):
        name = f"{name}-shell"
        features = "qemu boot-shell"
    if mode == "io-test-shell":
        name = f"{name}-io-test"
        features = f"{features} test-io-control"
    elif mode == "mm-performance-shell":
        name = f"{name}-mm-performance"
        features = (
            f"{features} test-io-control mm-lock-diagnostics "
            "asid-switch-diagnostics pmu-diagnostics"
        )
    make_args = ["PLAT_CONFIG=platforms/axplat-x86-q35-uefi.toml"]
    requested_cpus = requested_kernel_cpu_count()
    if requested_cpus is None:
        requested_cpus = 4
    make_args.append(f"SMP={requested_cpus}")
    if requested_cpus > 1:
        features = f"{features} smp"
    requested_memory = requested_kernel_memory()
    if requested_memory is not None:
        make_args.append(f"MEM={requested_memory}")
    if requested_asid_fast_switch():
        features = f"{features} asid-fast-switch"
    return KernelRequest(
        name=name,
        arch=full_arch,
        make_args=tuple(make_args),
        app_features=features,
        root=root,
    )


def kernel_build(req: KernelRequest, output: Path) -> None:
    root = req.root
    state = state_root(root)
    out_dir = state_path(root, req.arch, "out")
    common = [
        "make",
        "-C",
        str(root / "make"),
        f"A={root}",
        f"ARCH={req.arch}",
        *_kernel_make_var_args(req),
        f"STATE_DIR={state}",
    ]
    env = kernel_build_env(req)
    for goal in ("defconfig", "build-elf-fast"):
        argv = [*common, f"APP_FEATURES={req.app_features}", goal]
        completed = subprocess.run(argv, cwd=root, env=env, check=False)
        if completed.returncode != 0:
            raise BuildError(f"kernel build step failed ({goal}): {' '.join(argv)}")

    elfs = [path for path in out_dir.glob("*.elf") if path.is_file()]
    if not elfs:
        raise BuildError(f"no kernel ELF produced under {out_dir}")
    kernel_elf = max(elfs, key=lambda path: path.stat().st_mtime_ns)

    output.parent.mkdir(parents=True, exist_ok=True)
    # x86_64's linker output is already the bootable Multiboot ELF. Keep that
    # format intact; it does not need architecture-specific address rewriting.
    shutil.copy2(kernel_elf, output)

    stripped = output.with_name(output.name + ".stripped")
    strip_argv = ["rust-objcopy", "--strip-all", str(output), str(stripped)]
    completed = subprocess.run(strip_argv, cwd=root, env=env, check=False)
    if completed.returncode != 0:
        stripped.unlink(missing_ok=True)
        raise BuildError(f"rust-objcopy strip failed: {' '.join(strip_argv)}")
    stripped.replace(output)

    # The stripped ELF is the exact artifact GRUB loads from the ESP, so the
    # Multiboot1/2 header gate must run on it after stripping.
    multiboot_argv = [
        "bash",
        str(root / "scripts" / "check-x86-multiboot.sh"),
        str(output),
    ]
    completed = subprocess.run(
        multiboot_argv,
        cwd=root,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if completed.returncode != 0:
        detail = completed.stdout.strip() if completed.stdout else ""
        raise BuildError(
            f"x86_64 Multiboot header gate failed: {' '.join(multiboot_argv)}"
            + (f": {detail}" if detail else "")
        )


def build_x86_uefi_esp(*, root: Path, kernel: Path, output: Path, verbose: bool = False) -> None:
    """Build the GPT/FAT32 ESP consumed by the default x86_64 runner."""

    if not kernel.is_file():
        raise BuildError(f"x86_64 kernel does not exist: {kernel}")
    command = [
        "bash",
        str(root / "scripts" / "build-x86-uefi-esp.sh"),
        "--kernel",
        str(kernel),
        "--output",
        str(output),
    ]
    output.parent.mkdir(parents=True, exist_ok=True)
    completed = subprocess.run(
        command,
        cwd=root,
        env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        check=False,
        stdout=None if verbose else subprocess.PIPE,
        stderr=None if verbose else subprocess.STDOUT,
        text=True,
    )
    if completed.returncode != 0:
        detail = ""
        if not verbose and completed.stdout:
            detail = f": {completed.stdout.strip()}"
        raise BuildError(
            f"x86_64 ESP builder failed ({completed.returncode}): {' '.join(command)}{detail}"
        )


def _kernel_make_var_args(req: KernelRequest) -> list[str]:
    args = list(req.make_args)
    overridden = {argument.partition("=")[0] for argument in req.make_args if "=" in argument}
    for key, value in KERNEL_ENV.items():
        if value != "" and key not in overridden:
            args.append(f"{key}={value}")
    return args


def kernel_build_env(req: KernelRequest) -> dict[str, str]:
    env = os.environ.copy()
    state = state_root(req.root)
    env.update(KERNEL_ENV)
    for argument in req.make_args:
        key, separator, value = argument.partition("=")
        if separator:
            env[key] = value
    env["ARCH"] = req.arch
    env["APP_FEATURES"] = req.app_features
    env["THEKERNEL_STATE_DIR"] = str(state)
    env["STATE_DIR"] = str(state)
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return env


def ensure_kernel(
    *,
    mode: Literal[
        "release", "shell", "io-test-shell", "mm-performance-shell"
    ],
    arch: str,
    root: Path | None = None,
    output: Path | None = None,
    esp_output: Path | None = None,
    verbose: bool = False,
) -> BuildResult:
    root = (root or find_repo_root()).resolve()
    arch_alias = normalize_short_arch(arch)
    req = make_kernel_request(mode, arch_alias, root)
    result = ensure_cached_artifact(
        kind="kernel",
        name=req.name,
        params=kernel_params(req),
        input_specs=kernel_input_specs(req),
        cache_path_for_identity=lambda identity: kernel_cache_dir(root) / f"{req.name}-{identity[:16]}",
        build=lambda cache_path: kernel_build(req, cache_path),
        root=root,
        verbose=verbose,
    )
    if output is None:
        output_name = "kernel-x86_64"
        if mode == "release":
            output = root / output_name
        elif mode == "shell":
            output = state_path(root, "shell", output_name)
        elif mode == "io-test-shell":
            output = state_path(root, "io-test-shell", output_name)
        else:
            output = state_path(root, "mm-performance-shell", output_name)
    materialize_file(result.cache_path, output, prefer_hardlink=True, root=root)
    if esp_output is not None:
        esp_path = esp_output.expanduser()
        if not esp_path.is_absolute():
            esp_path = root / esp_path
        build_x86_uefi_esp(
            root=root, kernel=output, output=esp_path.resolve(), verbose=verbose
        )
    return BuildResult(result.kind, result.name, result.cache_path, output, result.identity, result.hit)


def rootfs_params(req: RootfsRequest) -> dict[str, str]:
    prefix_name = "THEKERNEL_X86_CROSS_COMPILE"
    default_prefix = "x86_64-linux-gnu-"
    prefix = os.environ.get(prefix_name, default_prefix)
    return {
        "arch": req.arch,
        "cross_compile": prefix,
        "cc": capture([f"{prefix}gcc", "--version"], max_lines=1),
        "mke2fs": capture(["mke2fs", "-V"], max_lines=2),
        "source_date_epoch": os.environ.get("SOURCE_DATE_EPOCH") or "1704067200",
    }


def rootfs_input_specs(_req: RootfsRequest) -> list[InputSpec]:
    return [
        InputSpec("file", "tools/build.py"),
        InputSpec("file", "scripts/build-rootfs.sh"),
        InputSpec("file", "scripts/create-rootfs-image.sh"),
        InputSpec("file", "tests/rootfs/busybox-1.36.1.config"),
        InputSpec("file", "LICENSE"),
        InputSpec("file", "NOTICE"),
        InputSpec("file", "PROVENANCE.md"),
        InputSpec("file", "dev-env/Dockerfile"),
        InputSpec("file", "dev-env/versions.env"),
        InputSpec("tree", "tests/guest"),
    ]


def rootfs_build(req: RootfsRequest, output: Path, *, verbose: bool = False) -> None:
    command = [
        "bash",
        str(req.root / "scripts" / "build-rootfs.sh"),
        "--arch",
        req.arch,
        "--output",
        str(output),
    ]
    output.parent.mkdir(parents=True, exist_ok=True)
    state = state_root(req.root)
    env = {
        **os.environ,
        "PYTHONDONTWRITEBYTECODE": "1",
        "THEKERNEL_STATE_DIR": str(state),
    }
    env.setdefault("THEKERNEL_SOURCE_CACHE", str(state_path(req.root, "source-cache")))
    env.setdefault(
        "THEKERNEL_ROOTFS_BUILD_DIR", str(state_path(req.root, "rootfs-build"))
    )
    completed = subprocess.run(
        command,
        cwd=req.root,
        env=env,
        check=False,
        text=True,
        stdout=None if verbose else subprocess.PIPE,
        stderr=None if verbose else subprocess.STDOUT,
    )
    if completed.returncode != 0:
        detail = ""
        if not verbose and completed.stdout:
            detail = f": {completed.stdout.strip()}"
        raise BuildError(
            f"rootfs builder failed ({completed.returncode}): {' '.join(command)}{detail}"
        )


def ensure_rootfs(
    *,
    arch: str,
    root: Path | None = None,
    output: Path | None = None,
    verbose: bool = False,
) -> BuildResult:
    root = (root or find_repo_root()).resolve()
    arch_alias = normalize_rootfs_arch(arch)
    req = RootfsRequest(arch=arch_alias, root=root)
    result = ensure_cached_artifact(
        kind="rootfs",
        name=f"rootfs-{arch_alias}",
        params=rootfs_params(req),
        input_specs=rootfs_input_specs(req),
        cache_path_for_identity=lambda identity: rootfs_cache_dir(root)
        / f"rootfs-{arch_alias}-{identity[:16]}.img",
        build=lambda cache_path: rootfs_build(req, cache_path, verbose=verbose),
        root=root,
        verbose=verbose,
    )
    if output is None:
        output = state_path(root, "rootfs", f"rootfs-{arch_alias}.img")
    materialize_file(result.cache_path, output, prefer_hardlink=False, root=root)
    return BuildResult(
        result.kind,
        result.name,
        result.cache_path,
        output,
        result.identity,
        result.hit,
    )


def materialize_file(source: Path, dest: Path, *, prefer_hardlink: bool, root: Path | None = None) -> None:
    source = source.resolve()
    base_root = (root or find_repo_root()).resolve()
    dest = dest.expanduser()
    dest = (base_root / dest).resolve() if not dest.is_absolute() else dest.resolve()
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.exists() or dest.is_symlink():
        try:
            if dest.samefile(source):
                return
        except OSError:
            pass
        if same_file_content(source, dest):
            return

    tmp = dest.with_name(f".{dest.name}.tmp.{os.getpid()}")
    tmp.unlink(missing_ok=True)
    try:
        if prefer_hardlink:
            try:
                os.link(source, tmp)
            except OSError:
                shutil.copy2(source, tmp)
        else:
            copy_reflink_or_copy(source, tmp)
        tmp.replace(dest)
    finally:
        tmp.unlink(missing_ok=True)


def same_file_content(left: Path, right: Path) -> bool:
    if not right.is_file():
        return False
    try:
        if left.stat().st_size != right.stat().st_size:
            return False
    except OSError:
        return False
    return sha256_file(left) == sha256_file(right)


def copy_reflink_or_copy(source: Path, dest: Path) -> None:
    cp = shutil.which("cp")
    if cp is not None:
        completed = subprocess.run(
            [cp, "--reflink=auto", "--preserve=mode,timestamps", str(source), str(dest)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if completed.returncode == 0:
            return
        dest.unlink(missing_ok=True)
    shutil.copy2(source, dest)


def capture(argv: list[str], *, max_lines: int | None = None) -> str:
    binary = argv[0]
    if shutil.which(binary) is None:
        return f"missing:{binary}"
    try:
        completed = subprocess.run(
            argv,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        )
    except OSError as error:
        return f"error:{binary}:{error}"
    text = completed.stdout or ""
    if max_lines is not None:
        text = "\n".join(text.splitlines()[:max_lines])
    return text.strip()


def normalize_short_arch(value: str) -> Literal["x86"]:
    if value in ("x86", "x86_64"):
        return "x86"
    raise ValueError(f"unsupported arch: {value}")


def normalize_rootfs_arch(value: str) -> Literal["x86"]:
    return normalize_short_arch(value)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="python3 tools/build.py")
    sub = parser.add_subparsers(dest="command", required=True)

    kernel = sub.add_parser("kernel", help="build or reuse a release kernel")
    kernel.add_argument("arch", choices=("x86", "x86_64"))
    kernel.add_argument(
        "--esp",
        help="for x86_64, build a GPT/FAT32 ESP at this path using the UEFI loader",
    )
    kernel.add_argument("--verbose", action="store_true")
    kernel.set_defaults(func=kernel_cmd)

    shell = sub.add_parser("shell", help="build/reuse interactive shell kernel")
    shell.add_argument("arch", choices=("x86", "x86_64"))
    shell.add_argument("--esp", help="for x86_64, build a GPT/FAT32 ESP at this path")
    shell.add_argument("--verbose", action="store_true")
    shell.set_defaults(func=shell_cmd)

    io_test_shell = sub.add_parser(
        "io-test-shell", help="build/reuse a test-only I/O control shell kernel"
    )
    io_test_shell.add_argument("arch", choices=("x86", "x86_64"))
    io_test_shell.add_argument("--esp", help="for x86_64, build a GPT/FAT32 ESP at this path")
    io_test_shell.add_argument("--verbose", action="store_true")
    io_test_shell.set_defaults(func=io_test_shell_cmd)

    mm_performance_shell = sub.add_parser(
        "mm-performance-shell",
        help="build/reuse an MM performance diagnostics shell kernel",
    )
    mm_performance_shell.add_argument("arch", choices=("x86", "x86_64"))
    mm_performance_shell.add_argument(
        "--esp", help="for x86_64, build a GPT/FAT32 ESP at this path"
    )
    mm_performance_shell.add_argument("--verbose", action="store_true")
    mm_performance_shell.set_defaults(func=mm_performance_shell_cmd)

    rootfs = sub.add_parser(
        "rootfs", help="build or reuse a project test root filesystem"
    )
    rootfs.add_argument("arch", choices=("x86", "x86_64"))
    rootfs.add_argument("--output", help="materialize the image at an explicit path")
    rootfs.add_argument("--verbose", action="store_true")
    rootfs.set_defaults(func=rootfs_cmd)

    return parser


def kernel_cmd(args: argparse.Namespace) -> int:
    result = ensure_kernel(
        mode="release",
        arch=args.arch,
        esp_output=Path(args.esp).expanduser() if args.esp else None,
        verbose=args.verbose,
    )
    if args.verbose:
        print(result.output_path)
    return 0


def shell_cmd(args: argparse.Namespace) -> int:
    result = ensure_kernel(
        mode="shell",
        arch=args.arch,
        esp_output=Path(args.esp).expanduser() if args.esp else None,
        verbose=args.verbose,
    )
    if args.verbose:
        print(result.output_path)
    return 0


def io_test_shell_cmd(args: argparse.Namespace) -> int:
    result = ensure_kernel(
        mode="io-test-shell",
        arch=args.arch,
        esp_output=Path(args.esp).expanduser() if args.esp else None,
        verbose=args.verbose,
    )
    if args.verbose:
        print(result.output_path)
    return 0


def mm_performance_shell_cmd(args: argparse.Namespace) -> int:
    result = ensure_kernel(
        mode="mm-performance-shell",
        arch=args.arch,
        esp_output=Path(args.esp).expanduser() if args.esp else None,
        verbose=args.verbose,
    )
    if args.verbose:
        print(result.output_path)
    return 0


def rootfs_cmd(args: argparse.Namespace) -> int:
    output = Path(args.output).expanduser() if args.output else None
    result = ensure_rootfs(arch=args.arch, output=output, verbose=args.verbose)
    if args.verbose:
        print(result.output_path)
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
        return int(args.func(args))
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130
    except (BuildError, ValueError, FileNotFoundError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
