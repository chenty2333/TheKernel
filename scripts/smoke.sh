#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
SMOKE_DIR="$REPO_ROOT/tests/guest/smoke"

usage() {
    cat <<'EOF'
Usage:
  scripts/smoke.sh list
  scripts/smoke.sh NAME [ARGS...]

Semantic smokes run their guest command stream through the product CLI.
Arguments after NAME are passed to `thekernel.py run`.

Smoke names:
EOF
    list_smokes
}

list_smokes() {
    local path name
    for path in "$SMOKE_DIR"/*.commands; do
        [ -f "$path" ] || continue
        name=${path##*/}
        printf '  %s\n' "${name%.commands}"
    done
}

if [ $# -eq 0 ]; then
    usage
    exit 0
fi

case "$1" in
    -h|--help)
        usage
        exit 0
        ;;
    list)
        printf 'Smoke names:\n'
        list_smokes
        exit 0
        ;;
esac

name=$1
shift
case "$name" in
    ''|*[!a-z0-9-]*) commands= ;;
    *) commands="$SMOKE_DIR/$name.commands" ;;
esac
if [ ! -f "$commands" ]; then
    printf 'scripts/smoke.sh: unknown smoke: %s\n' "$name" >&2
    usage >&2
    exit 2
fi

extra_args=()
case "$name" in
    async-block-queue|async-irq-first)
        state_dir=${THEKERNEL_STATE_DIR:-"$REPO_ROOT/.state"}
        case "$state_dir" in /*) ;; *) state_dir="$REPO_ROOT/$state_dir" ;; esac
        extra_block="$state_dir/out/smoke/$name-extra.img"
        mkdir -p "$(dirname -- "$extra_block")"
        truncate -s 8M "$extra_block"
        extra_args=(--extra-block "$extra_block")
        ;;
esac

cd "$REPO_ROOT"
exec python3 tools/thekernel.py run --profile io-test --commands "$commands" \
    --input-after-marker THEKERNEL_SHELL_READY \
    --stop-after-marker '# THEKERNEL_SMOKE_COMPLETE' \
    "${extra_args[@]}" "$@"
