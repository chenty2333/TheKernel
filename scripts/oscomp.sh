#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

REFERENCE_PLAN_GROUPS=(
    basic
    iozone
    busybox
    netperf
    lua
    libcbench
    libctest
    unixbench
    cyclictest
    lmbench
    iperf
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
  scripts/oscomp.sh lab ...
  scripts/oscomp.sh run --arch {rv|la|riscv64|loongarch64} [options]
  scripts/oscomp.sh verify --arch {rv|la|riscv64|loongarch64} [--image PATH]
  scripts/oscomp.sh validate-output --log PATH [--arch {rv|la|riscv64|loongarch64}]

Commands:
  list
      Print the current T202-aligned evaluation plan.

  lab
      Forward to scripts/ltp-lab.py for LTP inventory, focused list
      generation, replay orchestration, log parsing, and cleanup.

  run
      Boot the official pre-2025 evaluator image under the contest QEMU shape.
      The image search order is:
        $OSCOMP_TESTSUITE_DIR
        /home/dia/kernel-image
        $HOME/kernel-image
        $HOME/testsuits-for-oskernel
        /coursegrader/testdata
      Accepted image suffixes are .img, .img.xz, and .img.gz.

      Options:
        --arch VALUE
        --image PATH
        --support-image PATH
        --timeout SECS
        --workdir DIR
        --skip-kernel-build
        --keep-workdir

  verify
      Validate the expected internal pre-2025 image layout.

  validate-output
      Validate score-facing OSComp TEST GROUP markers in a replay or remote
      console log. This checks evaluator-visible group protocol separately
      from LTP case parsing.
EOF
}

list_cmd() {
    printf 'arches:\n'
    printf '  rv (riscv64)\n'
    printf '  la (loongarch64)\n'
    printf 'plan order:\n'
    printf '  /musl basic\n'
    printf '  /musl iozone\n'
    printf '  /musl busybox\n'
    printf '  /musl netperf\n'
    printf '  /musl lua\n'
    printf '  /musl libcbench\n'
    printf '  /musl libctest\n'
    printf '  /musl unixbench\n'
    printf '  /musl cyclictest\n'
    printf '  /glibc basic\n'
    printf '  /glibc iozone\n'
    printf '  /glibc busybox\n'
    printf '  /glibc netperf\n'
    printf '  /glibc lua\n'
    printf '  /glibc libcbench\n'
    printf '  /glibc unixbench\n'
    printf '  /glibc cyclictest\n'
    printf '  /musl lmbench\n'
    printf '  /glibc lmbench\n'
    printf '  /musl iperf\n'
    printf '  /glibc iperf\n'
    printf '  /glibc ltp\n'
    printf '  /musl ltp\n'
    printf 'groups in fixed plan:\n'
    printf '  %s\n' "${REFERENCE_PLAN_GROUPS[@]}"
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
            --image|--support-image|--timeout|--workdir)
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

validate_output_cmd() {
    local arch=""
    local args=()

    while (($#)); do
        case "$1" in
            --arch)
                [[ $# -ge 2 ]] || die "missing value for --arch"
                arch=$(canonical_arch "$2") || die "unsupported arch: $2"
                args+=("--arch" "$arch")
                shift 2
                ;;
            --log)
                [[ $# -ge 2 ]] || die "missing value for --log"
                args+=("--log" "$2")
                shift 2
                ;;
            --require-conclusion)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown validate-output option: $1"
                ;;
        esac
    done

    exec python3 "$SCRIPT_DIR/validate-oscomp-output.py" "${args[@]}"
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
        validate-output)
            shift
            validate_output_cmd "$@"
            ;;
        lab)
            shift
            exec "$SCRIPT_DIR/ltp-lab.py" "$@"
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
