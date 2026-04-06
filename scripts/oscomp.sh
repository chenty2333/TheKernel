#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

OFFICIAL_GROUPS=(
    basic
    busybox
    lua
    libctest
    iozone
    unixbench
    iperf
    libcbench
    lmbench
    netperf
    cyclictest
    ltp
)

die() {
    printf '[oscomp] error: %s\n' "$*" >&2
    exit 1
}

canonical_arch() {
    case "$1" in
        rv|riscv64)
            printf 'rv\n'
            ;;
        la|loongarch64)
            printf 'la\n'
            ;;
        *)
            return 1
            ;;
    esac
}

usage() {
    cat <<'EOF'
Usage:
  scripts/oscomp.sh list
  scripts/oscomp.sh run --arch {rv|la|riscv64|loongarch64} [options]
  scripts/oscomp.sh verify --arch {rv|la|riscv64|loongarch64} [--image PATH]

Commands:
  list
      Print the official pre-2025 arch/libc/group matrix.

  run
      Boot the official pre-2025 evaluator image under the exact QEMU command
      shape used by the contest. By default only
      ${OSCOMP_TESTSUITE_DIR:-~/testsuits-for-oskernel} is searched for
      sdcard-*.img{,.xz}.

      Options:
        --arch VALUE
        --image PATH
        --root {musl|glibc|all}
        --groups CSV
        --timeout SECS
        --workdir DIR
        --skip-kernel-build
        --keep-workdir

  verify
      Validate the expected internal pre-2025 image layout.
EOF
}

list_cmd() {
    printf 'arches:\n'
    printf '  rv (riscv64)\n'
    printf '  la (loongarch64)\n'
    printf 'libcs:\n'
    printf '  musl\n'
    printf '  glibc\n'
    printf '  all\n'
    printf 'groups:\n'
    printf '  %s\n' "${OFFICIAL_GROUPS[@]}"
}

run_cmd() {
    local arch=""
    local args=()

    while (($#)); do
        case "$1" in
            --arch)
                [[ $# -ge 2 ]] || die "missing value for --arch"
                arch=$(canonical_arch "$2") || die "unsupported arch: $2"
                shift 2
                ;;
            --image|--root|--groups|--timeout|--workdir)
                [[ $# -ge 2 ]] || die "missing value for $1"
                args+=("$1" "$2")
                shift 2
                ;;
            --skip-kernel-build|--keep-workdir)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown run option: $1"
                ;;
        esac
    done

    [[ -n "$arch" ]] || die "run requires --arch"
    exec "$SCRIPT_DIR/replay-oscomp-eval.sh" --arch "$arch" "${args[@]}"
}

verify_cmd() {
    local arch=""
    local args=()

    while (($#)); do
        case "$1" in
            --arch)
                [[ $# -ge 2 ]] || die "missing value for --arch"
                arch=$(canonical_arch "$2") || die "unsupported arch: $2"
                shift 2
                ;;
            --image)
                [[ $# -ge 2 ]] || die "missing value for --image"
                args+=("$1" "$2")
                shift 2
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown verify option: $1"
                ;;
        esac
    done

    [[ -n "$arch" ]] || die "verify requires --arch"
    exec "$SCRIPT_DIR/verify-pre2025-layout.sh" --arch "$arch" "${args[@]}"
}

main() {
    case "${1:-}" in
        list)
            shift
            list_cmd "$@"
            ;;
        run)
            shift
            run_cmd "$@"
            ;;
        verify)
            shift
            verify_cmd "$@"
            ;;
        ""|-h|--help|help)
            usage
            ;;
        *)
            die "unknown subcommand: $1"
            ;;
    esac
}

main "$@"
