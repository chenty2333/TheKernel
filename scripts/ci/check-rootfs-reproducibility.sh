#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

ARCHES=both
WORKDIR=

usage() {
    cat <<'EOF'
Usage: scripts/ci/check-rootfs-reproducibility.sh \
  [--arch {rv|la|both}] --workdir NEW_DIR

Build each selected rootfs twice with independent source caches, compiler work
directories, output paths, and temporary staging trees. The gate succeeds only
when both complete ext4 images have the same SHA-256 and byte content.
EOF
}

while (($#)); do
    case "$1" in
        --arch) ARCHES=${2:-}; shift 2 ;;
        --workdir) WORKDIR=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

case "$ARCHES" in
    rv|la|both) ;;
    *) printf '%s\n' '--arch must be rv, la, or both' >&2; exit 2 ;;
esac
[ -n "$WORKDIR" ] || { printf '%s\n' '--workdir is required' >&2; exit 2; }
case "$WORKDIR" in
    /*) ;;
    *) WORKDIR="$REPO_ROOT/$WORKDIR" ;;
esac
[ ! -e "$WORKDIR" ] \
    || { printf 'workdir must not already exist: %s\n' "$WORKDIR" >&2; exit 2; }
for command in cmp git sha256sum stat; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 78
    }
done
mkdir -p "$WORKDIR"

repo_commit=$(git -C "$REPO_ROOT" rev-parse --verify HEAD)
[ -z "$(git -C "$REPO_ROOT" status --porcelain --untracked-files=all)" ] \
    || { printf '%s\n' 'rootfs reproducibility evidence requires a clean worktree' >&2; exit 1; }
printf 'thekernel_commit\tarch\tsha256\tsize_bytes\timage_a\timage_b\n' \
    >"$WORKDIR/rootfs-reproducibility.tsv"

case "$ARCHES" in
    rv) selected=(rv) ;;
    la) selected=(la) ;;
    both) selected=(rv la) ;;
esac

for arch in "${selected[@]}"; do
    images=()
    for run in a b; do
        run_dir="$WORKDIR/$run-$arch"
        mkdir -p "$run_dir"
        image="$run_dir/rootfs-$arch.img"
        env \
            THEKERNEL_SOURCE_CACHE="$run_dir/source-cache" \
            THEKERNEL_ROOTFS_BUILD_DIR="$run_dir/compiler-work" \
            SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1704067200} \
            "$REPO_ROOT/scripts/build-rootfs.sh" \
                --arch "$arch" --output "$image" \
                >"$run_dir/build.log" 2>&1
        images+=("$image")
    done
    sha_a=$(sha256sum "${images[0]}" | awk '{print $1}')
    sha_b=$(sha256sum "${images[1]}" | awk '{print $1}')
    size_a=$(stat -c '%s' "${images[0]}")
    size_b=$(stat -c '%s' "${images[1]}")
    [ "$size_a" = "$size_b" ] && [ "$sha_a" = "$sha_b" ] \
        && cmp -s "${images[0]}" "${images[1]}" || {
            printf 'rootfs reproducibility mismatch: arch=%s sha_a=%s sha_b=%s\n' \
                "$arch" "$sha_a" "$sha_b" >&2
            exit 1
        }
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$repo_commit" "$arch" "$sha_a" "$size_a" \
        "a-$arch/rootfs-$arch.img" "b-$arch/rootfs-$arch.img" \
        >>"$WORKDIR/rootfs-reproducibility.tsv"
done

printf 'rootfs reproducibility: PASS receipt=%s\n' \
    "$WORKDIR/rootfs-reproducibility.tsv"
