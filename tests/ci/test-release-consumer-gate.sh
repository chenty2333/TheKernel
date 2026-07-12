#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
CI_DIR="$REPO_ROOT/scripts/ci"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

bash -n "$CI_DIR/release-consumer-gate.sh"
"$CI_DIR/release-consumer-gate.sh" --help >/dev/null

# The temporary-manifest rewrite is exact and refuses to proceed if an anchor
# disappeared or became ambiguous.
mkdir -p "$tmp/rewrite"
cat >"$tmp/rewrite/Cargo.toml" <<'EOF'
[workspace]
members = []

[workspace.dependencies]
one = { path = "../source/one" }

[patch.crates-io]
two = { path = "../source/two" }
EOF
python3 "$CI_DIR/rewrite-release-consumer.py" \
    --manifest "$tmp/rewrite/Cargo.toml" \
    --replace '../source/one=../artifacts/one-0.1.0' \
    --replace '../source/two=../artifacts/two-0.1.0' \
    --forbid-text '../source/' \
    --record "$tmp/rewrite/record.tsv" >/dev/null
grep -Fq 'path = "../artifacts/one-0.1.0"' "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/two-0.1.0"' "$tmp/rewrite/Cargo.toml"
grep -q $'^before_sha256\t[0-9a-f]\{64\}$' "$tmp/rewrite/record.tsv"
if python3 "$CI_DIR/rewrite-release-consumer.py" \
    --manifest "$tmp/rewrite/Cargo.toml" \
    --replace '../source/one=../artifacts/other' >/dev/null 2>&1; then
    printf 'test-release-consumer: missing rewrite anchor was accepted\n' >&2
    exit 1
fi

# Build small Cargo-shaped archives without invoking Cargo.  The auditor must
# accept a clean exact-HEAD archive and reject path dependencies and traversal.
fake_head=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
python3 - "$tmp" "$fake_head" <<'PY'
import io
import json
import pathlib
import tarfile
import sys

root = pathlib.Path(sys.argv[1])
head = sys.argv[2]
repository = "https://github.com/chenty2333/thekernel-ax"

def make_archive(
    name: str,
    manifest: str,
    *,
    traversal: bool = False,
    symlink: bool = False,
) -> None:
    archive_path = root / f"{name}.crate"
    package_root = "thekernel-test-0.1.0"
    with tarfile.open(archive_path, "w:gz") as archive:
        files = {
            f"{package_root}/Cargo.toml": manifest.encode(),
            f"{package_root}/.cargo_vcs_info.json": json.dumps(
                {"git": {"sha1": head}, "path_in_vcs": "crates/test"}
            ).encode(),
            f"{package_root}/src/lib.rs": b"#![no_std]\n",
        }
        if traversal:
            files[f"{package_root}/../escape"] = b"escape"
        for path, data in files.items():
            info = tarfile.TarInfo(path)
            info.size = len(data)
            archive.addfile(info, io.BytesIO(data))
        if symlink:
            info = tarfile.TarInfo(f"{package_root}/src/escape-link")
            info.type = tarfile.SYMTYPE
            info.linkname = "../../escape"
            archive.addfile(info)

valid = f'''[package]
name = "thekernel-test"
version = "0.1.0"
edition = "2024"
repository = "{repository}"

[dependencies]
spin = "0.10"
'''
make_archive("valid", valid)
make_archive(
    "path-leak",
    valid.replace('spin = "0.10"', 'spin = { version = "0.10", path = "../spin" }'),
)
make_archive(
    "registry-leak",
    valid.replace(
        'spin = "0.10"',
        'spin = { version = "0.10", registry = "private" }',
    ),
)
make_archive("traversal", valid, traversal=True)
make_archive("symlink", valid, symlink=True)
PY

artifact_record=$(python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/valid.crate" \
    --extract-root "$tmp/unpacked-valid" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" \
    --repository https://github.com/chenty2333/thekernel-ax)
artifact_checksum=$(printf '%s\n' "$artifact_record" | cut -f3)
[[ "$artifact_checksum" =~ ^[0-9a-f]{64}$ ]]
cat >"$tmp/Cargo.lock" <<EOF
version = 4

[[package]]
name = "thekernel-test"
version = "0.1.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "$artifact_checksum"
EOF
python3 "$CI_DIR/release-lock-artifacts.py" \
    --lock "$tmp/Cargo.lock" \
    --artifact "thekernel-test=$tmp/valid.crate" >/dev/null
sed 's/checksum = ".*"/checksum = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"/' \
    "$tmp/Cargo.lock" >"$tmp/wrong-Cargo.lock"
if python3 "$CI_DIR/release-lock-artifacts.py" \
    --lock "$tmp/wrong-Cargo.lock" \
    --artifact "thekernel-test=$tmp/valid.crate" >/dev/null 2>&1; then
    printf 'test-release-consumer: mismatched sibling lock checksum was accepted\n' >&2
    exit 1
fi

if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/path-leak.crate" \
    --extract-root "$tmp/unpacked-path-leak" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" >/dev/null 2>&1; then
    printf 'test-release-consumer: packaged path dependency was accepted\n' >&2
    exit 1
fi
if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/registry-leak.crate" \
    --extract-root "$tmp/unpacked-registry-leak" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" >/dev/null 2>&1; then
    printf 'test-release-consumer: alternate-registry dependency was accepted\n' >&2
    exit 1
fi
if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/traversal.crate" \
    --extract-root "$tmp/unpacked-traversal" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" >/dev/null 2>&1; then
    printf 'test-release-consumer: archive traversal was accepted\n' >&2
    exit 1
fi
if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/symlink.crate" \
    --extract-root "$tmp/unpacked-symlink" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" >/dev/null 2>&1; then
    printf 'test-release-consumer: archive symlink was accepted\n' >&2
    exit 1
fi
if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/valid.crate" \
    --extract-root "$tmp/unpacked-size-limit" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" \
    --max-unpacked-bytes 1 >/dev/null 2>&1; then
    printf 'test-release-consumer: unpacked-size limit was ignored\n' >&2
    exit 1
fi
if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/valid.crate" \
    --extract-root "$tmp/unpacked-wrong-checksum" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" \
    --expected-sha256 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
    >/dev/null 2>&1; then
    printf 'test-release-consumer: wrong expected checksum was accepted\n' >&2
    exit 1
fi
if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/valid.crate" \
    --extract-root "$tmp/unpacked-wrong-head" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb \
    >/dev/null 2>&1; then
    printf 'test-release-consumer: wrong archive HEAD was accepted\n' >&2
    exit 1
fi
if python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/valid.crate" \
    --extract-root "$tmp/unpacked-wrong-version" \
    --package thekernel-test \
    --version 0.1.1 \
    --repo-head "$fake_head" >/dev/null 2>&1; then
    printf 'test-release-consumer: wrong package version was accepted\n' >&2
    exit 1
fi

# Exercise dependency-graph identity checks with paths containing no real
# release workspaces.  A sensitive fake source verifies diagnostics never echo
# registry or Git source values.
mkdir -p \
    "$tmp/consumer/crates/axtask-compat" \
    "$tmp/artifacts/thekernel-axsched-0.1.0" \
    "$tmp/artifacts/thekernel-axpoll-0.1.0" \
    "$tmp/artifacts/thekernel-axtask-0.1.0" \
    "$tmp/artifacts/thekernel-linux-vfs-0.1.0" \
    "$tmp/artifacts/thekernel-linux-fd-0.1.0" \
    "$tmp/source-ax" "$tmp/source-linux-abi"

python3 - "$tmp" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
release_names = [
    "thekernel-axsched",
    "thekernel-axpoll",
    "thekernel-axtask",
    "thekernel-linux-vfs",
    "thekernel-linux-fd",
]
packages = [
    {
        "id": "consumer 0.1.0 (path+consumer)",
        "name": "consumer",
        "version": "0.1.0",
        "source": None,
        "manifest_path": str(root / "consumer/Cargo.toml"),
    },
    {
        "id": "axtask 0.3.0 (path+facade)",
        "name": "axtask",
        "version": "0.3.0-preview.2",
        "source": None,
        "manifest_path": str(root / "consumer/crates/axtask-compat/Cargo.toml"),
        "publish": [],
        "dependencies": [
            {
                "name": "thekernel-axtask",
                "rename": "axtask-core",
                "req": "=0.1.0",
            }
        ],
    },
]
for name in release_names:
    packages.append(
        {
            "id": f"{name} 0.1.0 (path+artifact)",
            "name": name,
            "version": "0.1.0",
            "source": None,
            "manifest_path": str(root / f"artifacts/{name}-0.1.0/Cargo.toml"),
        }
    )
dependencies = [package["id"] for package in packages[1:]]
nodes = [{"id": packages[0]["id"], "dependencies": dependencies}]
nodes.extend({"id": package["id"], "dependencies": []} for package in packages[1:])
metadata = {
    "packages": packages,
    "workspace_members": [packages[0]["id"]],
    "resolve": {"root": packages[0]["id"], "nodes": nodes},
}
(root / "metadata.json").write_text(json.dumps(metadata))
PY

graph_args=(
    --metadata "$tmp/metadata.json"
    --consumer-root "$tmp/consumer"
    --allowed-axtask-facade "$tmp/consumer/crates/axtask-compat"
    --release-source-root "$tmp/source-ax"
    --release-source-root "$tmp/source-linux-abi"
)
for package in \
    thekernel-axsched thekernel-axpoll thekernel-axtask \
    thekernel-linux-vfs thekernel-linux-fd; do
    graph_args+=(--expect "$package=$tmp/artifacts/$package-0.1.0")
done
python3 "$CI_DIR/release-dependency-graph.py" "${graph_args[@]}" >/dev/null

python3 - "$tmp/metadata.json" "$tmp/legacy-metadata.json" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
legacy = {
    "id": "axpoll 0.1.2 (non-local)",
    "name": "axpoll",
    "version": "0.1.2",
    "source": "SENSITIVE_SOURCE_VALUE_MUST_NOT_BE_ECHOED",
    "manifest_path": "/registry/axpoll-0.1.2/Cargo.toml",
}
metadata["packages"].append(legacy)
metadata["resolve"]["nodes"].append({"id": legacy["id"], "dependencies": []})
metadata["resolve"]["nodes"][0]["dependencies"].append(legacy["id"])
pathlib.Path(sys.argv[2]).write_text(json.dumps(metadata))
PY
legacy_args=("${graph_args[@]}")
legacy_args[1]="$tmp/legacy-metadata.json"
if python3 "$CI_DIR/release-dependency-graph.py" "${legacy_args[@]}" \
    >"$tmp/legacy.log" 2>&1; then
    printf 'test-release-consumer: legacy registry axpoll was accepted\n' >&2
    exit 1
fi
if grep -Fq 'SENSITIVE_SOURCE_VALUE_MUST_NOT_BE_ECHOED' "$tmp/legacy.log"; then
    printf 'test-release-consumer: dependency diagnostic leaked credentials\n' >&2
    exit 1
fi

python3 - "$tmp/metadata.json" "$tmp/source-leak.json" "$tmp/source-ax" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
for package in metadata["packages"]:
    if package["name"] == "thekernel-axpoll":
        package["manifest_path"] = str(
            pathlib.Path(sys.argv[3]) / "crates/thekernel-axpoll/Cargo.toml"
        )
pathlib.Path(sys.argv[2]).write_text(json.dumps(metadata))
PY
source_args=("${graph_args[@]}")
source_args[1]="$tmp/source-leak.json"
if python3 "$CI_DIR/release-dependency-graph.py" "${source_args[@]}" \
    >/dev/null 2>&1; then
    printf 'test-release-consumer: source-workspace artifact leak was accepted\n' >&2
    exit 1
fi

python3 - "$tmp/metadata.json" "$tmp/vendor-leak.json" "$tmp/consumer" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
for package in metadata["packages"]:
    if package["name"] == "axtask":
        package["manifest_path"] = str(
            pathlib.Path(sys.argv[3])
            / "third_party/rust-patches/axtask/Cargo.toml"
        )
pathlib.Path(sys.argv[2]).write_text(json.dumps(metadata))
PY
vendor_args=("${graph_args[@]}")
vendor_args[1]="$tmp/vendor-leak.json"
if python3 "$CI_DIR/release-dependency-graph.py" "${vendor_args[@]}" \
    >/dev/null 2>&1; then
    printf 'test-release-consumer: legacy vendored axtask was accepted\n' >&2
    exit 1
fi

printf 'test-release-consumer-gate: PASS\n'
