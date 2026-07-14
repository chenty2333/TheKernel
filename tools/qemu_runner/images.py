"""Compressed raw-image preparation with a caller-owned cache directory."""

from __future__ import annotations

import gzip
import hashlib
import lzma
import os
import shutil
import time
from pathlib import Path

from .model import PreparedImage


class ImageError(RuntimeError):
    """Raised when an image cannot be validated or prepared."""


def _cache_key(source: Path) -> str:
    stat = source.stat()
    digest = hashlib.sha256()
    digest.update(str(source.resolve()).encode())
    digest.update(b"\0")
    digest.update(str(stat.st_size).encode())
    digest.update(b"\0")
    digest.update(str(stat.st_mtime_ns).encode())
    return digest.hexdigest()


def _is_compressed(source: Path) -> bool:
    return source.name.endswith((".xz", ".gz"))


def _runtime_name(source: Path) -> str:
    if source.name.endswith(".xz"):
        return source.name.removesuffix(".xz")
    if source.name.endswith(".gz"):
        return source.name.removesuffix(".gz")
    return source.name


def _stale_lock(lock_dir: Path, *, stale_after_secs: float) -> bool:
    try:
        return time.time() - lock_dir.stat().st_mtime > stale_after_secs
    except OSError:
        return False


def _decompress(source: Path, target: Path) -> None:
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(f".{target.name}.tmp.{os.getpid()}")
    temporary.unlink(missing_ok=True)
    try:
        if source.name.endswith(".xz"):
            opener = lzma.open
        elif source.name.endswith(".gz"):
            opener = gzip.open
        else:
            raise ImageError(f"unsupported compressed image: {source}")
        with opener(source, "rb") as input_file, temporary.open("wb") as output_file:
            shutil.copyfileobj(input_file, output_file, length=1024 * 1024)
        if temporary.stat().st_size == 0:
            raise ImageError(f"decompressed image is empty: {source}")
        temporary.replace(target)
    except ImageError:
        temporary.unlink(missing_ok=True)
        raise
    except Exception as error:
        temporary.unlink(missing_ok=True)
        raise ImageError(f"could not decompress image {source}: {error}") from error


def prepare_image(
    source: Path,
    *,
    cache_dir: Path,
    lock_wait_timeout_secs: float = 120.0,
    stale_lock_secs: float = 30.0 * 60.0,
) -> PreparedImage:
    """Return an uncompressed image path, reusing an atomic cache when needed."""

    source = source.expanduser().resolve()
    if not source.is_file():
        raise ImageError(f"image does not exist: {source}")
    if source.stat().st_size == 0:
        raise ImageError(f"image is empty: {source}")
    if not _is_compressed(source):
        return PreparedImage(source=source, runtime=source, cached=False)

    cache_dir = cache_dir.expanduser().resolve()
    entry_dir = cache_dir / _cache_key(source)
    target = entry_dir / _runtime_name(source)
    lock_dir = entry_dir.with_suffix(".lock")
    cache_dir.mkdir(parents=True, exist_ok=True)
    if target.is_file() and target.stat().st_size > 0:
        return PreparedImage(source=source, runtime=target, cached=True)

    wait_started = time.monotonic()
    while True:
        try:
            lock_dir.mkdir()
            break
        except FileExistsError:
            if target.is_file() and target.stat().st_size > 0:
                return PreparedImage(source=source, runtime=target, cached=True)
            if _stale_lock(lock_dir, stale_after_secs=stale_lock_secs):
                try:
                    lock_dir.rmdir()
                    continue
                except OSError:
                    pass
            if time.monotonic() - wait_started >= lock_wait_timeout_secs:
                raise ImageError(f"timed out waiting for image-cache lock: {lock_dir}")
            time.sleep(0.05)

    try:
        if not target.is_file() or target.stat().st_size == 0:
            _decompress(source, target)
        return PreparedImage(source=source, runtime=target, cached=True)
    finally:
        try:
            lock_dir.rmdir()
        except OSError:
            pass


def materialize_writable_image(
    prepared: PreparedImage,
    *,
    destination_dir: Path,
    label: str,
) -> Path:
    """Keep a shared decompression cache immutable for writable attachments."""

    if not prepared.cached:
        return prepared.runtime
    destination_dir.mkdir(parents=True, exist_ok=True)
    safe_label = "".join(char if char.isalnum() or char in "-_" else "_" for char in label)
    destination = destination_dir / f"{safe_label}-{prepared.runtime.name}"
    temporary = destination.with_name(f".{destination.name}.tmp.{os.getpid()}")
    temporary.unlink(missing_ok=True)
    try:
        shutil.copy2(prepared.runtime, temporary)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination
