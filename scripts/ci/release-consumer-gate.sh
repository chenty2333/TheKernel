#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"

AX_REPO=${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}
LINUX_ABI_REPO=${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}
AX_EXPECTED_HEAD=${THEKERNEL_AX_EXPECTED_HEAD:-}
LINUX_ABI_EXPECTED_HEAD=${THEKERNEL_LINUX_ABI_EXPECTED_HEAD:-}
EXPECTED_RELEASE_SET=${THEKERNEL_EXPECTED_RELEASE_SET:-}
OUTPUT_RELEASE_SET=${THEKERNEL_OUTPUT_RELEASE_SET:-$REPO_ROOT/.state/ci/release-consumer/release-set.tsv}
WORK_ROOT=${THEKERNEL_RELEASE_GATE_WORK_ROOT:-$REPO_ROOT/.state/ci/release-consumer-work}
ARCHES=${THEKERNEL_RELEASE_GATE_ARCHES:-both}
PACKAGE_TOOLCHAIN=${THEKERNEL_RELEASE_PACKAGE_TOOLCHAIN:-nightly-2025-05-20}

VERSION=0.1.0
AX_REPOSITORY=https://github.com/chenty2333/thekernel-ax
LINUX_ABI_REPOSITORY=https://github.com/chenty2333/thekernel-linux-abi
AX_PACKAGES=(
    thekernel-axsched
    thekernel-axpoll
    thekernel-axcbpf
    thekernel-axfault
    thekernel-axtask
    thekernel-axtlb
)
LINUX_ABI_PACKAGES=(
    thekernel-linux-usercopy
    thekernel-linux-cred
    thekernel-linux-mm
    thekernel-linux-io-uring
    thekernel-linux-seccomp
    thekernel-linux-process
    thekernel-linux-signal
    thekernel-linux-vfs
    thekernel-linux-fd
)
CONSUMED_PACKAGES=(
    thekernel-axsched
    thekernel-axpoll
    thekernel-axcbpf
    thekernel-axfault
    thekernel-axtask
    thekernel-axtlb
    thekernel-linux-cred
    thekernel-linux-mm
    thekernel-linux-io-uring
    thekernel-linux-seccomp
    thekernel-linux-process
    thekernel-linux-vfs
    thekernel-linux-fd
)

usage() {
    cat <<'EOF'
Usage: scripts/ci/release-consumer-gate.sh [OPTIONS]

Packages the clean thekernel-ax and thekernel-linux-abi release workspaces,
validates and safely extracts the exact .crate archives, retargets a temporary
copy of TheKernel to those artifacts, rejects legacy or source-workspace
instances, and builds the RISC-V and LoongArch release-mode kernels.

Options:
  --ax-repo DIR                 thekernel-ax checkout
  --linux-abi-repo DIR          thekernel-linux-abi checkout
  --ax-head COMMIT              require this exact 40-hex ax HEAD
  --linux-abi-head COMMIT       require this exact 40-hex Linux ABI HEAD
  --expected-release-set FILE   require package checksums and HEADs from FILE
  --output-release-set FILE     write the verified release-set TSV here
  --work-root DIR               parent for bounded temporary work directories
  --arch rv|la|both             consumer build architecture set (default both)
  --package-toolchain NAME      Cargo toolchain for workspace packaging
  -h, --help                    show this help

Release-set rows are: package, version, sha256, repository_head.  The first
run can create the file; an exact release rerun should pass it back through
--expected-release-set.
EOF
}

while (($#)); do
    case "$1" in
        --ax-repo) AX_REPO=${2:-}; shift 2 ;;
        --linux-abi-repo) LINUX_ABI_REPO=${2:-}; shift 2 ;;
        --ax-head) AX_EXPECTED_HEAD=${2:-}; shift 2 ;;
        --linux-abi-head) LINUX_ABI_EXPECTED_HEAD=${2:-}; shift 2 ;;
        --expected-release-set) EXPECTED_RELEASE_SET=${2:-}; shift 2 ;;
        --output-release-set) OUTPUT_RELEASE_SET=${2:-}; shift 2 ;;
        --work-root) WORK_ROOT=${2:-}; shift 2 ;;
        --arch) ARCHES=${2:-}; shift 2 ;;
        --package-toolchain) PACKAGE_TOOLCHAIN=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) ci_die "unknown release-consumer argument: $1" ;;
    esac
done

case "$ARCHES" in
    rv|la|both) ;;
    *) ci_die "--arch must be rv, la, or both: $ARCHES" ;;
esac
[ -n "$PACKAGE_TOOLCHAIN" ] || ci_die '--package-toolchain must not be empty'
[[ "$PACKAGE_TOOLCHAIN" =~ ^[A-Za-z0-9._-]+$ ]] \
    || ci_die '--package-toolchain contains unsafe characters'

for command in cargo git make mktemp python3 rsync sha256sum; do
    ci_require_command "$command"
done

canonical_directory() {
    local path=$1
    [ -d "$path" ] || ci_die "required directory does not exist: $path"
    (cd -- "$path" && pwd -P)
}

validate_commit() {
    local label=$1
    local commit=$2
    [[ "$commit" =~ ^[0-9a-f]{40}$ ]] \
        || ci_die "$label must be an exact lowercase 40-hex commit"
}

verify_release_repo() {
    local label=$1
    local repo=$2
    local expected_head=$3
    local top_level head

    top_level=$(git -C "$repo" rev-parse --show-toplevel 2>/dev/null) \
        || ci_die "$label is not a Git worktree"
    top_level=$(canonical_directory "$top_level")
    [ "$top_level" = "$repo" ] \
        || ci_die "$label path is not its Git worktree root"
    if ! git -C "$repo" diff --quiet --ignore-submodules=none \
        || ! git -C "$repo" diff --cached --quiet --ignore-submodules=none \
        || [ -n "$(git -C "$repo" ls-files --others --exclude-standard)" ]; then
        ci_die "$label release worktree is not clean"
    fi

    head=$(git -C "$repo" rev-parse --verify HEAD^{commit} 2>/dev/null) \
        || ci_die "$label has no valid HEAD commit"
    validate_commit "$label HEAD" "$head"
    if [ -n "$expected_head" ]; then
        validate_commit "$label expected HEAD" "$expected_head"
        [ "$head" = "$expected_head" ] \
            || ci_die "$label HEAD does not match the pinned release commit"
    fi
    printf '%s\n' "$head"
}

AX_REPO=$(canonical_directory "$AX_REPO")
LINUX_ABI_REPO=$(canonical_directory "$LINUX_ABI_REPO")
[ "$AX_REPO" != "$LINUX_ABI_REPO" ] \
    || ci_die 'ax and Linux ABI repositories must be distinct worktrees'
AX_HEAD=$(verify_release_repo thekernel-ax "$AX_REPO" "$AX_EXPECTED_HEAD")
LINUX_ABI_HEAD=$(
    verify_release_repo thekernel-linux-abi "$LINUX_ABI_REPO" \
        "$LINUX_ABI_EXPECTED_HEAD"
)

declare -A EXPECTED_CHECKSUMS=()
declare -A EXPECTED_HEADS=()
declare -A SEEN_RELEASE_ROWS=()
all_packages=("${AX_PACKAGES[@]}" "${LINUX_ABI_PACKAGES[@]}")

is_release_package() {
    local candidate=$1
    local known
    for known in "${all_packages[@]}"; do
        [ "$candidate" != "$known" ] || return 0
    done
    return 1
}

if [ -n "$EXPECTED_RELEASE_SET" ]; then
    [ -f "$EXPECTED_RELEASE_SET" ] \
        || ci_die "expected release set does not exist: $EXPECTED_RELEASE_SET"
    while IFS=$'\t' read -r package version checksum repo_head extra; do
        [ -n "$package" ] || continue
        [ "$package" != package ] || continue
        is_release_package "$package" \
            || ci_die "expected release set contains unknown package: $package"
        [ -z "${extra:-}" ] || ci_die "invalid release-set row for $package"
        [[ "$checksum" =~ ^[0-9a-f]{64}$ ]] \
            || ci_die "invalid release-set checksum for $package"
        validate_commit "release-set HEAD for $package" "$repo_head"
        [ "$version" = "$VERSION" ] \
            || ci_die "release-set version for $package is not $VERSION"
        [ -z "${SEEN_RELEASE_ROWS[$package]:-}" ] \
            || ci_die "duplicate release-set package: $package"
        SEEN_RELEASE_ROWS[$package]=1
        EXPECTED_CHECKSUMS[$package]=$checksum
        EXPECTED_HEADS[$package]=$repo_head
    done <"$EXPECTED_RELEASE_SET"
    for package in "${all_packages[@]}"; do
        [ -n "${SEEN_RELEASE_ROWS[$package]:-}" ] \
            || ci_die "expected release set is missing $package"
    done
    [ "${#SEEN_RELEASE_ROWS[@]}" -eq "${#all_packages[@]}" ] \
        || ci_die 'expected release set contains an unknown package'
    for package in "${AX_PACKAGES[@]}"; do
        [ "${EXPECTED_HEADS[$package]}" = "$AX_HEAD" ] \
            || ci_die "release-set HEAD mismatch for $package"
    done
    for package in "${LINUX_ABI_PACKAGES[@]}"; do
        [ "${EXPECTED_HEADS[$package]}" = "$LINUX_ABI_HEAD" ] \
            || ci_die "release-set HEAD mismatch for $package"
    done
fi

mkdir -p "$WORK_ROOT"
WORK_ROOT=$(canonical_directory "$WORK_ROOT")
[ "$WORK_ROOT" != / ] || ci_die 'release work root must not be the filesystem root'
[ "$WORK_ROOT" != "$REPO_ROOT" ] \
    || ci_die 'release work root must not be the consumer repository root'
work_dir=$(mktemp -d -- "$WORK_ROOT/run.XXXXXXXX")
work_marker="$work_dir/.thekernel-release-consumer-work"
: >"$work_marker"

cleanup() {
    local status=$?
    trap - EXIT INT TERM
    case "$work_dir" in
        "$WORK_ROOT"/run.*)
            if [ -f "$work_marker" ]; then
                rm -rf -- "$work_dir"
            else
                printf 'ci: refusing to clean unmarked release work directory\n' >&2
                [ "$status" -ne 0 ] || status=2
            fi
            ;;
        *)
            printf 'ci: refusing to clean release work outside configured root\n' >&2
            [ "$status" -ne 0 ] || status=2
            ;;
    esac
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

archive_root="$work_dir/archives"
artifact_root="$work_dir/artifacts"
consumer_root="$work_dir/consumer"
package_source_root="$work_dir/package-sources"
mkdir -p \
    "$archive_root/ax" \
    "$archive_root/linux-abi" \
    "$artifact_root" \
    "$consumer_root" \
    "$package_source_root"

# Cargo only emits .cargo_vcs_info.json when the package path has a complete
# repository identity. A linked worktree bind-mounted at a container alias can
# pass Git's HEAD checks while Cargo sees no VCS root, changing the release
# bytes. Package from detached clones of the already-verified clean sources so
# normal checkouts, linked worktrees, and container aliases are reproducible.
clone_release_source() {
    local label=$1
    local source=$2
    local expected_head=$3
    local destination=$4
    local cloned_head

    git clone --quiet --no-checkout --no-local -- "$source" "$destination" \
        || ci_die "failed to clone verified $label source"
    git -C "$destination" checkout --quiet --detach "$expected_head" \
        || ci_die "failed to check out verified $label HEAD"
    cloned_head=$(git -C "$destination" rev-parse --verify HEAD^{commit}) \
        || ci_die "cloned $label source has no valid HEAD"
    [ "$cloned_head" = "$expected_head" ] \
        || ci_die "cloned $label HEAD changed before packaging"
    [ -z "$(git -C "$destination" status --porcelain=v1 --untracked-files=all)" ] \
        || ci_die "cloned $label source is not clean"
}

ax_package_repo="$package_source_root/thekernel-ax"
linux_abi_package_repo="$package_source_root/thekernel-linux-abi"
clone_release_source thekernel-ax "$AX_REPO" "$AX_HEAD" "$ax_package_repo"
clone_release_source \
    thekernel-linux-abi \
    "$LINUX_ABI_REPO" \
    "$LINUX_ABI_HEAD" \
    "$linux_abi_package_repo"

printf '[release-consumer] package thekernel-ax at %.12s\n' "$AX_HEAD"
(
    cd "$ax_package_repo"
    CARGO_TARGET_DIR="$archive_root/ax" \
        cargo "+$PACKAGE_TOOLCHAIN" -Z package-workspace package \
            --locked --no-verify \
            -p thekernel-axsched \
            -p thekernel-axpoll \
            -p thekernel-axcbpf \
            -p thekernel-axfault \
            -p thekernel-axtask \
            -p thekernel-axtlb
)

# `cargo package --no-verify` still resolves registry dependencies while it
# normalizes release locks.  On the first coordinated release, dependent
# packages therefore cannot be assembled from crates.io until their exact
# sibling archives have been published.  Package dependency roots first, then
# build a temporary Cargo directory source containing all locked registry
# inputs plus those audited archives.  This source exists only to model the
# dependency-order boundary before publication: normalized manifests and locks
# still name crates.io, and their checksums are checked against these archives
# below.
prepublish_source="$work_dir/prepublish-directory-source"
prepublish_source_config="$work_dir/prepublish-directory-source.config.toml"
axcbpf_archive="$archive_root/ax/package/thekernel-axcbpf-$VERSION.crate"
[ -f "$axcbpf_archive" ] \
    || ci_die "thekernel-axcbpf package archive is missing before dependent packaging"

printf '[release-consumer] package Linux-ABI dependency roots at %.12s\n' \
    "$LINUX_ABI_HEAD"
(
    cd "$linux_abi_package_repo"
    CARGO_TARGET_DIR="$archive_root/linux-abi" \
        cargo "+$PACKAGE_TOOLCHAIN" package \
            --locked --no-verify \
            -p thekernel-linux-usercopy
)
usercopy_archive="$archive_root/linux-abi/package/thekernel-linux-usercopy-$VERSION.crate"
[ -f "$usercopy_archive" ] \
    || ci_die "thekernel-linux-usercopy package archive is missing before dependent packaging"

cargo "+$PACKAGE_TOOLCHAIN" vendor \
    --manifest-path "$linux_abi_package_repo/Cargo.toml" \
    --locked \
    --versioned-dirs \
    "$prepublish_source" >"$prepublish_source_config"

stage_prepublish_archive() {
    local archive=$1
    local package=$2
    local repo_head=$3
    local repository=$4

    python3 "$SCRIPT_DIR/release-consumer-artifact.py" \
        --archive "$archive" \
        --extract-root "$prepublish_source" \
        --package "$package" \
        --version "$VERSION" \
        --repo-head "$repo_head" \
        --repository "$repository" \
        --directory-source-checksum >/dev/null
}

stage_prepublish_archive \
    "$axcbpf_archive" thekernel-axcbpf "$AX_HEAD" "$AX_REPOSITORY"
stage_prepublish_archive \
    "$usercopy_archive" thekernel-linux-usercopy \
    "$LINUX_ABI_HEAD" "$LINUX_ABI_REPOSITORY"
prepublish_cargo_config=(
    --config
    "$prepublish_source_config"
)

printf '[release-consumer] package thekernel-linux-abi at %.12s\n' \
    "$LINUX_ABI_HEAD"
(
    cd "$linux_abi_package_repo"
    CARGO_TARGET_DIR="$archive_root/linux-abi" \
        cargo "+$PACKAGE_TOOLCHAIN" -Z package-workspace package \
            --locked --offline --no-verify --registry crates-io \
            -p thekernel-linux-usercopy \
            -p thekernel-linux-cred \
            -p thekernel-linux-mm \
            -p thekernel-linux-io-uring \
            -p thekernel-linux-seccomp \
            -p thekernel-linux-process \
            -p thekernel-linux-signal \
            -p thekernel-linux-vfs \
            -p thekernel-linux-fd \
            "${prepublish_cargo_config[@]}"
)

declare -A ARTIFACT_DIRS=()
declare -A ARCHIVE_PATHS=()
declare -A ARCHIVE_CHECKSUMS=()

audit_archive() {
    local package=$1
    local archive=$2
    local repo_head=$3
    local repository=$4
    local -a checksum_arg=()
    local record checksum package_dir

    [ -f "$archive" ] || ci_die "package archive missing for $package"
    if [ -n "${EXPECTED_CHECKSUMS[$package]:-}" ]; then
        checksum_arg=(--expected-sha256 "${EXPECTED_CHECKSUMS[$package]}")
    fi
    record=$(
        python3 "$SCRIPT_DIR/release-consumer-artifact.py" \
            --archive "$archive" \
            --extract-root "$artifact_root" \
            --package "$package" \
            --version "$VERSION" \
            --repo-head "$repo_head" \
            --repository "$repository" \
            "${checksum_arg[@]}"
    )
    IFS=$'\t' read -r _ _ checksum package_dir <<<"$record"
    [[ "$checksum" =~ ^[0-9a-f]{64}$ ]] \
        || ci_die "artifact audit returned an invalid checksum for $package"
    [ -d "$package_dir" ] \
        || ci_die "artifact audit returned a missing directory for $package"
    ARTIFACT_DIRS[$package]=$package_dir
    ARCHIVE_PATHS[$package]=$archive
    ARCHIVE_CHECKSUMS[$package]=$checksum
}

for package in "${AX_PACKAGES[@]}"; do
    audit_archive \
        "$package" \
        "$archive_root/ax/package/$package-$VERSION.crate" \
        "$AX_HEAD" \
        "$AX_REPOSITORY"
done
for package in "${LINUX_ABI_PACKAGES[@]}"; do
    audit_archive \
        "$package" \
        "$archive_root/linux-abi/package/$package-$VERSION.crate" \
        "$LINUX_ABI_HEAD" \
        "$LINUX_ABI_REPOSITORY"
done

# Nightly's workspace packager records not-yet-published siblings as crates.io
# packages.  Bind those lock checksums to the exact archives from this run so
# axtask and signal cannot be tested against a different local sibling tree.
python3 "$SCRIPT_DIR/release-lock-artifacts.py" \
    --lock "${ARTIFACT_DIRS[thekernel-axtask]}/Cargo.lock" \
    --artifact "thekernel-axsched=${ARCHIVE_PATHS[thekernel-axsched]}" \
    --artifact "thekernel-axpoll=${ARCHIVE_PATHS[thekernel-axpoll]}"
python3 "$SCRIPT_DIR/release-lock-artifacts.py" \
    --lock "${ARTIFACT_DIRS[thekernel-linux-signal]}/Cargo.lock" \
    --artifact \
        "thekernel-linux-usercopy=${ARCHIVE_PATHS[thekernel-linux-usercopy]}"
python3 "$SCRIPT_DIR/release-lock-artifacts.py" \
    --lock "${ARTIFACT_DIRS[thekernel-linux-seccomp]}/Cargo.lock" \
    --artifact "thekernel-axcbpf=${ARCHIVE_PATHS[thekernel-axcbpf]}"

# Copy the live integration state, including its intentional uncommitted slice,
# but never copy Git metadata, build outputs, CI state, or release workspaces.
# Only this disposable manifest is rewritten below.
rsync_args=(
    -a
    --exclude='/.git/'
    --exclude='/.state/'
    --exclude='/target/'
    --exclude='/kernel-rv'
    --exclude='/kernel-la'
)
case "$WORK_ROOT" in
    "$REPO_ROOT"/*)
        # Prevent a caller-selected in-repository work root from recursively
        # copying the run that is currently being assembled.
        work_relative=${WORK_ROOT#"$REPO_ROOT"/}
        rsync_args+=(--exclude="/$work_relative/")
        ;;
esac
rsync "${rsync_args[@]}" "$REPO_ROOT/" "$consumer_root/"

rewrite_record="$work_dir/manifest-rewrite.tsv"
python3 "$SCRIPT_DIR/rewrite-release-consumer.py" \
    --manifest "$consumer_root/Cargo.toml" \
    --replace "../thekernel-ax/crates/thekernel-axsched=../artifacts/thekernel-axsched-$VERSION" \
    --replace "../thekernel-ax/crates/thekernel-axpoll=../artifacts/thekernel-axpoll-$VERSION" \
    --replace "../thekernel-ax/crates/thekernel-axcbpf=../artifacts/thekernel-axcbpf-$VERSION" \
    --replace "../thekernel-ax/crates/thekernel-axfault=../artifacts/thekernel-axfault-$VERSION" \
    --replace "../thekernel-ax/crates/thekernel-axtask=../artifacts/thekernel-axtask-$VERSION" \
    --replace "../thekernel-ax/crates/thekernel-axtlb=../artifacts/thekernel-axtlb-$VERSION" \
    --replace "../thekernel-linux-abi/crates/cred=../artifacts/thekernel-linux-cred-$VERSION" \
    --replace "../thekernel-linux-abi/crates/mm=../artifacts/thekernel-linux-mm-$VERSION" \
    --replace "../thekernel-linux-abi/crates/io-uring=../artifacts/thekernel-linux-io-uring-$VERSION" \
    --replace "../thekernel-linux-abi/crates/seccomp=../artifacts/thekernel-linux-seccomp-$VERSION" \
    --replace "../thekernel-linux-abi/crates/process=../artifacts/thekernel-linux-process-$VERSION" \
    --replace "../thekernel-linux-abi/crates/vfs=../artifacts/thekernel-linux-vfs-$VERSION" \
    --replace "../thekernel-linux-abi/crates/fd=../artifacts/thekernel-linux-fd-$VERSION" \
    --forbid-text '../thekernel-ax/' \
    --forbid-text '../thekernel-linux-abi/' \
    --record "$rewrite_record"

lock_before=$(sha256sum "$consumer_root/Cargo.lock" | awk '{print $1}')
metadata_path="$work_dir/consumer-metadata.json"
(
    cd "$consumer_root"
    cargo metadata --locked --format-version 1 --features qemu >"$metadata_path"
)
lock_after_metadata=$(sha256sum "$consumer_root/Cargo.lock" | awk '{print $1}')
[ "$lock_after_metadata" = "$lock_before" ] \
    || ci_die 'Cargo metadata changed the temporary consumer lockfile'

graph_args=(
    --metadata "$metadata_path"
    --consumer-root "$consumer_root"
    --allowed-axtask-facade "$consumer_root/crates/axtask-compat"
    --allowed-process-adapter "$consumer_root/crates/process-adapter"
    --release-source-root "$AX_REPO"
    --release-source-root "$LINUX_ABI_REPO"
)
for package in "${CONSUMED_PACKAGES[@]}"; do
    graph_args+=(--expect "$package=${ARTIFACT_DIRS[$package]}")
done
python3 "$SCRIPT_DIR/release-dependency-graph.py" "${graph_args[@]}"

# Fetch the locked graph before forcing the actual kernel builds offline.  The
# wrapper adds --locked to the Makefile's cargo build invocation without
# modifying the real or copied Makefiles.
(cd "$consumer_root" && cargo fetch --locked)
real_cargo=$(command -v cargo)
wrapper_bin="$work_dir/locked-bin"
mkdir -p "$wrapper_bin"
cat >"$wrapper_bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
for argument in "$@"; do
    if [ "$argument" = build ]; then
        exec "$THEKERNEL_RELEASE_REAL_CARGO" "$@" --locked
    fi
done
exec "$THEKERNEL_RELEASE_REAL_CARGO" "$@"
EOF
chmod +x "$wrapper_bin/cargo"

build_arch() {
    local arch=$1
    local goal output
    case "$arch" in
        rv) goal=kernel-rv; output=kernel-rv ;;
        la) goal=kernel-la; output=kernel-la ;;
        *) ci_die "internal unsupported build architecture: $arch" ;;
    esac
    printf '[release-consumer] build %s from exact extracted artifacts\n' "$arch"
    (
        cd "$consumer_root"
        PATH="$wrapper_bin:$PATH" \
        THEKERNEL_RELEASE_REAL_CARGO="$real_cargo" \
        CARGO_NET_OFFLINE=true \
            make --no-print-directory "$goal"
    )
    [ -s "$consumer_root/$output" ] \
        || ci_die "$arch consumer build did not produce $output"
}

case "$ARCHES" in
    rv) build_arch rv ;;
    la) build_arch la ;;
    both) build_arch rv; build_arch la ;;
esac

lock_after_build=$(sha256sum "$consumer_root/Cargo.lock" | awk '{print $1}')
[ "$lock_after_build" = "$lock_before" ] \
    || ci_die 'consumer build changed the temporary lockfile'

for package in "${all_packages[@]}"; do
    checksum=$(sha256sum "${ARCHIVE_PATHS[$package]}" | awk '{print $1}')
    [ "$checksum" = "${ARCHIVE_CHECKSUMS[$package]}" ] \
        || ci_die "release archive changed during consumer builds: $package"
done

# A concurrent mutation after packaging must not turn an earlier archive into
# an apparent release of a newer worktree state.
[ "$(verify_release_repo thekernel-ax "$AX_REPO" "$AX_HEAD")" = "$AX_HEAD" ] \
    || ci_die 'thekernel-ax changed during the release consumer gate'
[ "$(verify_release_repo \
    thekernel-linux-abi "$LINUX_ABI_REPO" "$LINUX_ABI_HEAD")" = \
    "$LINUX_ABI_HEAD" ] \
    || ci_die 'thekernel-linux-abi changed during the release consumer gate'

case "$OUTPUT_RELEASE_SET" in
    /*) ;;
    *) OUTPUT_RELEASE_SET="$REPO_ROOT/$OUTPUT_RELEASE_SET" ;;
esac
mkdir -p "$(dirname -- "$OUTPUT_RELEASE_SET")"
release_set_tmp="$OUTPUT_RELEASE_SET.tmp.$$"
{
    printf 'package\tversion\tsha256\trepository_head\n'
    for package in "${all_packages[@]}"; do
        case "$package" in
            thekernel-axsched|thekernel-axpoll|thekernel-axcbpf|thekernel-axfault|thekernel-axtask|thekernel-axtlb)
                repo_head=$AX_HEAD
                ;;
            *)
                repo_head=$LINUX_ABI_HEAD
                ;;
        esac
        printf '%s\t%s\t%s\t%s\n' \
            "$package" "$VERSION" "${ARCHIVE_CHECKSUMS[$package]}" "$repo_head"
    done
} >"$release_set_tmp"
mv -f -- "$release_set_tmp" "$OUTPUT_RELEASE_SET"

printf 'release-consumer gate: PASS (%s; ax %.12s; linux-abi %.12s)\n' \
    "$ARCHES" "$AX_HEAD" "$LINUX_ABI_HEAD"
printf 'release set: %s\n' "$OUTPUT_RELEASE_SET"
