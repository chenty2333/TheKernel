#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
CI_DIR="$REPO_ROOT/scripts/ci"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

bash -n "$CI_DIR/release-consumer-gate.sh"
"$CI_DIR/release-consumer-gate.sh" --help >/dev/null
grep -Fqx \
    'PACKAGE_TOOLCHAIN=${THEKERNEL_RELEASE_PACKAGE_TOOLCHAIN:-nightly}' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fqx \
    'export AX_LINKER_SCRIPT_OUTPUT := $(abspath $(LD_SCRIPT))' \
    "$REPO_ROOT/make/cargo.mk"
grep -Fq \
    'std::env::var_os("AX_LINKER_SCRIPT_OUTPUT")' \
    "$REPO_ROOT/third_party/rust-patches/axhal/build.rs"
grep -Fq \
    '.find(|path| path.file_name() == Some(profile.as_ref()))' \
    "$REPO_ROOT/third_party/rust-patches/axhal/build.rs"
if [ "$(grep -c '^    thekernel-axfault$' \
    "$CI_DIR/release-consumer-gate.sh")" -lt 2 ]; then
    printf 'test-release-consumer: axfault is absent from a release package set\n' >&2
    exit 1
fi
if [ "$(grep -c '^    thekernel-linux-signal$' \
    "$CI_DIR/release-consumer-gate.sh")" -lt 2 ]; then
    printf 'test-release-consumer: signal is absent from a release consumer set\n' >&2
    exit 1
fi
grep -Fq -- '-p thekernel-axfault \' "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- '-p thekernel-axpmu \' "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- \
    '--replace "../thekernel-ax/crates/thekernel-axfault=../artifacts/thekernel-axfault-$VERSION" \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- \
    '--replace "../thekernel-ax/crates/thekernel-axpmu=../artifacts/thekernel-axpmu-$VERSION" \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    'thekernel-axsched|thekernel-axpoll|thekernel-axcbpf|thekernel-axfault|thekernel-axpmu|thekernel-axtask|thekernel-axtlb)' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- '-p thekernel-axcbpf \' "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- '-p thekernel-linux-seccomp \' "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- '-p thekernel-linux-packet \' "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- '-p thekernel-linux-rseq \' "$CI_DIR/release-consumer-gate.sh"
grep -Fq 'stage_prepublish_archive' "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    '"$usercopy_archive" thekernel-linux-usercopy \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    '"$rseq_archive"' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    '"../thekernel-linux-abi/crates/signal=../artifacts/thekernel-linux-signal-$VERSION" \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    '"../thekernel-linux-abi/crates/usercopy=../artifacts/thekernel-linux-usercopy-$VERSION" \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    '"../thekernel-linux-abi/crates/rseq=../artifacts/thekernel-linux-rseq-$VERSION" \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    '"${ARTIFACT_DIRS[thekernel-linux-signal]}/Cargo.lock"' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq \
    '"thekernel-linux-usercopy=${ARCHIVE_PATHS[thekernel-linux-usercopy]}"' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- '--locked --offline --no-verify --registry crates-io \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- 'exec "$THEKERNEL_RELEASE_REAL_CARGO" "$@" --locked --offline' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- '--features qemu,asid-switch-diagnostics,pmu-diagnostics \' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- 'goal=kernel-x86_64-mm-performance' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- 'build_diagnostics_arch x86_64' \
    "$CI_DIR/release-consumer-gate.sh"
grep -Fq -- 'CARGO_NET_OFFLINE=true \' "$CI_DIR/release-consumer-gate.sh"

# The temporary-manifest rewrite is exact and refuses to proceed if an anchor
# disappeared or became ambiguous.
mkdir -p "$tmp/rewrite"
cat >"$tmp/rewrite/Cargo.toml" <<'EOF'
[workspace]
members = []

[workspace.dependencies]
one = { path = "../source/one" }
axfault = { package = "thekernel-axfault", path = "../thekernel-ax/crates/thekernel-axfault" }
axpmu = { package = "thekernel-axpmu", path = "../thekernel-ax/crates/thekernel-axpmu" }
axcbpf = { package = "thekernel-axcbpf", path = "../thekernel-ax/crates/thekernel-axcbpf" }
axtlb = { package = "thekernel-axtlb", path = "../thekernel-ax/crates/thekernel-axtlb" }
thekernel-linux-cred = { path = "../thekernel-linux-abi/crates/cred" }
thekernel-linux-mm = { path = "../thekernel-linux-abi/crates/mm" }
thekernel-linux-packet = { path = "../thekernel-linux-abi/crates/packet" }
thekernel-linux-io-uring = { path = "../thekernel-linux-abi/crates/io-uring" }
thekernel-linux-seccomp = { path = "../thekernel-linux-abi/crates/seccomp" }
thekernel-linux-rseq = { path = "../thekernel-linux-abi/crates/rseq" }
thekernel-linux-signal = { path = "../thekernel-linux-abi/crates/signal" }
thekernel-linux-usercopy = { path = "../thekernel-linux-abi/crates/usercopy" }

[patch.crates-io]
two = { path = "../source/two" }
EOF
python3 "$CI_DIR/rewrite-release-consumer.py" \
    --manifest "$tmp/rewrite/Cargo.toml" \
    --replace '../source/one=../artifacts/one-0.1.0' \
    --replace '../source/two=../artifacts/two-0.1.0' \
    --replace '../thekernel-ax/crates/thekernel-axfault=../artifacts/thekernel-axfault-0.1.0' \
    --replace '../thekernel-ax/crates/thekernel-axpmu=../artifacts/thekernel-axpmu-0.1.0' \
    --replace '../thekernel-ax/crates/thekernel-axcbpf=../artifacts/thekernel-axcbpf-0.1.0' \
    --replace '../thekernel-ax/crates/thekernel-axtlb=../artifacts/thekernel-axtlb-0.1.0' \
    --replace '../thekernel-linux-abi/crates/cred=../artifacts/thekernel-linux-cred-0.1.0' \
    --replace '../thekernel-linux-abi/crates/mm=../artifacts/thekernel-linux-mm-0.1.0' \
    --replace '../thekernel-linux-abi/crates/packet=../artifacts/thekernel-linux-packet-0.1.0' \
    --replace '../thekernel-linux-abi/crates/io-uring=../artifacts/thekernel-linux-io-uring-0.1.0' \
    --replace '../thekernel-linux-abi/crates/seccomp=../artifacts/thekernel-linux-seccomp-0.1.0' \
    --replace '../thekernel-linux-abi/crates/rseq=../artifacts/thekernel-linux-rseq-0.1.0' \
    --replace '../thekernel-linux-abi/crates/signal=../artifacts/thekernel-linux-signal-0.1.0' \
    --replace '../thekernel-linux-abi/crates/usercopy=../artifacts/thekernel-linux-usercopy-0.1.0' \
    --forbid-text '../source/' \
    --forbid-text '../thekernel-ax/' \
    --forbid-text '../thekernel-linux-abi/' \
    --record "$tmp/rewrite/record.tsv" >/dev/null
grep -Fq 'path = "../artifacts/one-0.1.0"' "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/two-0.1.0"' "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-axfault-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-axpmu-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-axcbpf-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-axtlb-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-cred-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-mm-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-packet-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-io-uring-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-seccomp-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-rseq-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-signal-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
grep -Fq 'path = "../artifacts/thekernel-linux-usercopy-0.1.0"' \
    "$tmp/rewrite/Cargo.toml"
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
directory_record=$(python3 "$CI_DIR/release-consumer-artifact.py" \
    --archive "$tmp/valid.crate" \
    --extract-root "$tmp/directory-source" \
    --package thekernel-test \
    --version 0.1.0 \
    --repo-head "$fake_head" \
    --repository https://github.com/chenty2333/thekernel-ax \
    --directory-source-checksum)
directory_checksum=$(printf '%s\n' "$directory_record" | cut -f3)
python3 - \
    "$tmp/directory-source/thekernel-test-0.1.0/.cargo-checksum.json" \
    "$directory_checksum" <<'PY'
import json
import pathlib
import sys

record = json.loads(pathlib.Path(sys.argv[1]).read_text())
if record.get("package") != sys.argv[2]:
    raise SystemExit("directory-source package checksum does not bind the archive")
files = record.get("files")
if not isinstance(files, dict) or "Cargo.toml" not in files or "src/lib.rs" not in files:
    raise SystemExit("directory-source checksum does not cover packaged files")
if ".cargo-checksum.json" in files:
    raise SystemExit("directory-source checksum recursively covers itself")
PY
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
    "$tmp/consumer/crates/process-adapter" \
    "$tmp/artifacts/thekernel-axsched-0.1.0" \
    "$tmp/artifacts/thekernel-axpoll-0.1.0" \
    "$tmp/artifacts/thekernel-axcbpf-0.1.0" \
    "$tmp/artifacts/thekernel-axfault-0.1.0" \
    "$tmp/artifacts/thekernel-axpmu-0.1.0" \
    "$tmp/artifacts/thekernel-axtask-0.1.0" \
    "$tmp/artifacts/thekernel-axtlb-0.1.0" \
    "$tmp/artifacts/thekernel-linux-usercopy-0.1.0" \
    "$tmp/artifacts/thekernel-linux-rseq-0.1.0" \
    "$tmp/artifacts/thekernel-linux-cred-0.1.0" \
    "$tmp/artifacts/thekernel-linux-mm-0.1.0" \
    "$tmp/artifacts/thekernel-linux-packet-0.1.0" \
    "$tmp/artifacts/thekernel-linux-io-uring-0.1.0" \
    "$tmp/artifacts/thekernel-linux-seccomp-0.1.0" \
    "$tmp/artifacts/thekernel-linux-process-0.1.0" \
    "$tmp/artifacts/thekernel-linux-signal-0.1.0" \
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
    "thekernel-axcbpf",
    "thekernel-axfault",
    "thekernel-axpmu",
    "thekernel-axtask",
    "thekernel-axtlb",
    "thekernel-linux-usercopy",
    "thekernel-linux-rseq",
    "thekernel-linux-cred",
    "thekernel-linux-mm",
    "thekernel-linux-packet",
    "thekernel-linux-io-uring",
    "thekernel-linux-seccomp",
    "thekernel-linux-process",
    "thekernel-linux-signal",
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
        "id": "thekernel-linux-process-adapter 0.1.0 (path+adapter)",
        "name": "thekernel-linux-process-adapter",
        "version": "0.1.0",
        "source": None,
        "manifest_path": str(
            root / "consumer/crates/process-adapter/Cargo.toml"
        ),
        "publish": [],
        "dependencies": [
            {
                "name": "thekernel-linux-process",
                "rename": None,
                "req": "=0.1.0",
                "uses_default_features": False,
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
process_adapter_id = next(
    package["id"]
    for package in packages
    if package["name"] == "thekernel-linux-process-adapter"
)
process_core_id = next(
    package["id"]
    for package in packages
    if package["name"] == "thekernel-linux-process"
)
next(node for node in nodes if node["id"] == process_adapter_id)["dependencies"] = [
    process_core_id
]
signal_id = next(
    package["id"]
    for package in packages
    if package["name"] == "thekernel-linux-signal"
)
usercopy_id = next(
    package["id"]
    for package in packages
    if package["name"] == "thekernel-linux-usercopy"
)
next(node for node in nodes if node["id"] == signal_id)["dependencies"] = [
    usercopy_id
]
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
    --allowed-process-adapter "$tmp/consumer/crates/process-adapter"
    --release-source-root "$tmp/source-ax"
    --release-source-root "$tmp/source-linux-abi"
)
for package in \
    thekernel-axsched thekernel-axpoll thekernel-axcbpf thekernel-axfault \
    thekernel-axpmu thekernel-axtask thekernel-axtlb \
    thekernel-linux-usercopy thekernel-linux-rseq \
    thekernel-linux-cred thekernel-linux-mm thekernel-linux-packet \
    thekernel-linux-io-uring \
    thekernel-linux-seccomp \
    thekernel-linux-process \
    thekernel-linux-signal \
    thekernel-linux-vfs thekernel-linux-fd; do
    graph_args+=(--expect "$package=$tmp/artifacts/$package-0.1.0")
done
python3 "$CI_DIR/release-dependency-graph.py" "${graph_args[@]}" >/dev/null

python3 - "$tmp/metadata.json" "$tmp" <<'PY'
import copy
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
root = pathlib.Path(sys.argv[2])
adapter_name = "thekernel-linux-process-adapter"

def adapter(package_set):
    return next(package for package in package_set if package["name"] == adapter_name)

def write_variant(name, value):
    (root / f"adapter-{name}.json").write_text(json.dumps(value))

wrong_path = copy.deepcopy(metadata)
adapter(wrong_path["packages"])["manifest_path"] = str(
    root / "consumer/crates/not-process-adapter/Cargo.toml"
)
write_variant("wrong-path", wrong_path)

publishable = copy.deepcopy(metadata)
adapter(publishable["packages"])["publish"] = None
write_variant("publishable", publishable)

bad_dependency = copy.deepcopy(metadata)
adapter(bad_dependency["packages"])["dependencies"][0]["rename"] = "process-core"
write_variant("bad-dependency", bad_dependency)

unreachable = copy.deepcopy(metadata)
adapter_id = adapter(unreachable["packages"])["id"]
unreachable["resolve"]["nodes"][0]["dependencies"].remove(adapter_id)
write_variant("unreachable", unreachable)

missing = copy.deepcopy(metadata)
adapter_id = adapter(missing["packages"])["id"]
missing["packages"] = [
    package for package in missing["packages"] if package["id"] != adapter_id
]
missing["resolve"]["nodes"] = [
    node for node in missing["resolve"]["nodes"] if node["id"] != adapter_id
]
missing["resolve"]["nodes"][0]["dependencies"].remove(adapter_id)
write_variant("missing", missing)

duplicate = copy.deepcopy(metadata)
second_adapter = copy.deepcopy(adapter(duplicate["packages"]))
second_adapter["id"] = (
    "thekernel-linux-process-adapter 0.1.0 (path+duplicate-adapter)"
)
second_adapter["manifest_path"] = str(
    root / "consumer/crates/duplicate-process-adapter/Cargo.toml"
)
duplicate["packages"].append(second_adapter)
duplicate["resolve"]["nodes"].append(
    {"id": second_adapter["id"], "dependencies": []}
)
duplicate["resolve"]["nodes"][0]["dependencies"].append(second_adapter["id"])
write_variant("duplicate", duplicate)
PY
for adapter_case in \
    wrong-path publishable bad-dependency unreachable missing duplicate; do
    adapter_args=("${graph_args[@]}")
    adapter_args[1]="$tmp/adapter-$adapter_case.json"
    if python3 "$CI_DIR/release-dependency-graph.py" "${adapter_args[@]}" \
        >/dev/null 2>&1; then
        printf 'test-release-consumer: invalid process adapter %s was accepted\n' \
            "$adapter_case" >&2
        exit 1
    fi
done

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
legacy_axtask = {
    "id": "axtask 0.3.0 (non-local)",
    "name": "axtask",
    "version": "0.3.0-preview.2",
    "source": "SENSITIVE_SOURCE_VALUE_MUST_NOT_BE_ECHOED",
    "manifest_path": "/registry/axtask-0.3.0-preview.2/Cargo.toml",
}
metadata["packages"].append(legacy)
metadata["resolve"]["nodes"].append({"id": legacy["id"], "dependencies": []})
metadata["resolve"]["nodes"][0]["dependencies"].append(legacy["id"])
metadata["packages"].append(legacy_axtask)
metadata["resolve"]["nodes"].append(
    {"id": legacy_axtask["id"], "dependencies": []}
)
metadata["resolve"]["nodes"][0]["dependencies"].append(legacy_axtask["id"])
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

python3 - "$tmp/metadata.json" "$tmp/duplicate-process.json" <<'PY'
import json
import pathlib
import sys

metadata = json.loads(pathlib.Path(sys.argv[1]).read_text())
duplicate = {
    "id": "thekernel-linux-process 0.1.0 (path+duplicate)",
    "name": "thekernel-linux-process",
    "version": "0.1.0",
    "source": None,
    "manifest_path": "/unexpected/thekernel-linux-process/Cargo.toml",
}
metadata["packages"].append(duplicate)
metadata["resolve"]["nodes"].append({"id": duplicate["id"], "dependencies": []})
metadata["resolve"]["nodes"][0]["dependencies"].append(duplicate["id"])
pathlib.Path(sys.argv[2]).write_text(json.dumps(metadata))
PY
duplicate_process_args=("${graph_args[@]}")
duplicate_process_args[1]="$tmp/duplicate-process.json"
if python3 "$CI_DIR/release-dependency-graph.py" "${duplicate_process_args[@]}" \
    >/dev/null 2>&1; then
    printf 'test-release-consumer: duplicate Linux process core was accepted\n' >&2
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
    if package["name"] == "thekernel-axtask":
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
