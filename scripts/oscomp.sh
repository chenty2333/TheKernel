#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)

REFERENCE_PLAN_GROUPS=(
    basic
    busybox
    libctest
    lua
    iperf
    cyclictest
    netperf
    libcbench
    iozone
    lmbench
    ltp
)

die() {
    printf '[oscomp] error: %s\n' "$*" >&2
    exit 2
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
  scripts/oscomp.sh judge-log --arch {rv|la|riscv64|loongarch64} --log PATH --out DIR [--plan PATH] [options]
  scripts/oscomp.sh score-logs [--rv-log PATH] [--la-log PATH] [--plan PATH] [--name NAME] [--out DIR] [options]
  scripts/oscomp.sh evaluate [--rv-log PATH] [--la-log PATH] [--arch {rv|la|both}] [--plan PATH] [--ltp-list PATH] [options]
  scripts/oscomp.sh report-run RUN_DIR
  scripts/oscomp.sh inspect-run [--json] RUN_DIR
  scripts/oscomp.sh official-refresh --source PATH [--repo URL] [--commit SHA] [--allow-dirty]
  scripts/oscomp.sh support-check --arch {rv|la|riscv64|loongarch64} --image PATH [--json]

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

  judge-log
      Parse one console log, write marker artifacts, and run the vendored
      official-compatible judge scripts for the expected local matrix.
      Pass --plan PATH for focused logs that intentionally contain a reduced
      group/libc matrix.

  score-logs
      Offline score path for existing RV/LA logs. This creates a local run
      directory with marker, judge, manifest, and score JSON artifacts.
      Pass --plan PATH when the logs were generated from a matching reduced
      guest plan.

  report-run
      Regenerate report.md from an existing local run directory.

  inspect-run
      Inspect an existing local run directory without mutating it. This checks
      manifest, score, report, and artifact-index structure and returns
      nonzero when structural or score-facing issues are found.

  evaluate
      New local evaluation entrypoint. With --rv-log/--la-log it scores
      existing logs; without logs it launches replay through the existing
      replay-oscomp-eval.sh runner and then judges, scores, and reports.
      In replay mode, --plan PATH defines the expected matrix; use it with a
      matching --support-image when running a focused guest plan. Pass
      --ltp-list PATH to build a run-local support image with that LTP list.
      Pass --idle-timeout SECS to stop a replay that stops writing console
      output even though the whole-QEMU timeout has not expired.

  official-refresh
      Refresh the vendored official judge snapshot from an explicit local
      autotest-for-oskernel checkout. This command does not fetch from the
      network and does not import the official QEMU/prework/postwork controller.

  support-check
      Validate a support disk image before replay. This catches stale images
      whose /meta/init.sh no longer matches src/init.sh, or images missing
      guest-side timeout support required for bounded LTP runs.
EOF
}

list_cmd() {
    printf 'arches:\n'
    printf '  rv (riscv64)\n'
    printf '  la (loongarch64)\n'
    printf 'plan order:\n'
    printf '  /musl basic\n'
    printf '  /glibc basic\n'
    printf '  /musl busybox\n'
    printf '  /glibc busybox\n'
    printf '  /musl libctest\n'
    printf '  /musl lua\n'
    printf '  /glibc lua\n'
    printf '  /musl iperf\n'
    printf '  /glibc iperf\n'
    printf '  /musl netperf\n'
    printf '  /glibc netperf\n'
    printf '  /musl libcbench\n'
    printf '  /glibc libcbench\n'
    printf '  /musl iozone\n'
    printf '  /glibc iozone\n'
    printf '  /musl lmbench\n'
    printf '  /glibc lmbench\n'
    printf '  /glibc ltp\n'
    printf '  /musl ltp\n'
    printf '  /musl cyclictest\n'
    printf '  /glibc cyclictest\n'
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

    exec env OSCOMP_VALIDATE_OUTPUT_WRAPPER=1 python3 "$SCRIPT_DIR/validate-oscomp-output.py" "${args[@]}"
}

judge_log_cmd() {
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
            --log|--out|--judge-dir|--judge-timeout|--plan)
                [[ $# -ge 2 ]] || die "missing value for $1"
                args+=("$1" "$2")
                shift 2
                ;;
            --fail-fast)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown judge-log option: $1"
                ;;
        esac
    done

    [[ -n "$arch" ]] || die "judge-log requires --arch"
    exec env PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m tools.oscomp_eval judge-log "${args[@]}"
}

score_logs_cmd() {
    local args=()

    while (($#)); do
        case "$1" in
            --rv-log|--la-log|--name|--out|--judge-dir|--judge-timeout|--plan)
                [[ $# -ge 2 ]] || die "missing value for $1"
                args+=("$1" "$2")
                shift 2
                ;;
            --fail-fast|--replace)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown score-logs option: $1"
                ;;
        esac
    done

    exec env PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m tools.oscomp_eval score-logs "${args[@]}"
}

evaluate_cmd() {
    local args=()

    while (($#)); do
        case "$1" in
            --rv-log|--la-log|--name|--out|--judge-dir|--judge-timeout|--arch|--timeout|--idle-timeout|--image|--support-image|--plan|--ltp-list)
                [[ $# -ge 2 ]] || die "missing value for $1"
                args+=("$1" "$2")
                shift 2
                ;;
            --fail-fast|--replace|--skip-kernel-build|--keep-workdir)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown evaluate option: $1"
                ;;
        esac
    done

    exec env PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m tools.oscomp_eval evaluate "${args[@]}"
}

report_run_cmd() {
    [[ $# -eq 1 ]] || die "report-run requires exactly one RUN_DIR"
    exec env PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m tools.oscomp_eval report-run "$1"
}

inspect_run_cmd() {
    local args=()

    while (($#)); do
        case "$1" in
            --json)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            -*)
                die "unknown inspect-run option: $1"
                ;;
            *)
                args+=("$1")
                shift
                ;;
        esac
    done

    [[ ${#args[@]} -ge 1 ]] || die "inspect-run requires exactly one RUN_DIR"
    local run_dir_count=0
    for arg in "${args[@]}"; do
        [[ "$arg" == --json ]] || ((run_dir_count += 1))
    done
    [[ $run_dir_count -eq 1 ]] || die "inspect-run requires exactly one RUN_DIR"
    exec env PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m tools.oscomp_eval inspect-run "${args[@]}"
}

official_refresh_cmd() {
    local args=()

    while (($#)); do
        case "$1" in
            --source|--repo|--commit)
                [[ $# -ge 2 ]] || die "missing value for $1"
                args+=("$1" "$2")
                shift 2
                ;;
            --allow-dirty)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown official-refresh option: $1"
                ;;
        esac
    done

    exec env PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m tools.oscomp_eval official-refresh "${args[@]}"
}

support_check_cmd() {
    local args=()

    while (($#)); do
        case "$1" in
            --arch)
                [[ $# -ge 2 ]] || die "missing value for --arch"
                local arch
                arch=$(canonical_arch "$2") || die "unsupported arch: $2"
                args+=("--arch" "$arch")
                shift 2
                ;;
            --image)
                [[ $# -ge 2 ]] || die "missing value for --image"
                args+=("$1" "$2")
                shift 2
                ;;
            --json)
                args+=("$1")
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "unknown support-check option: $1"
                ;;
        esac
    done

    exec env PYTHONPATH="$REPO_ROOT${PYTHONPATH:+:$PYTHONPATH}" python3 -m tools.oscomp_eval support-check "${args[@]}"
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
        judge-log)
            shift
            judge_log_cmd "$@"
            ;;
        score-logs)
            shift
            score_logs_cmd "$@"
            ;;
        evaluate)
            shift
            evaluate_cmd "$@"
            ;;
        report-run)
            shift
            report_run_cmd "$@"
            ;;
        inspect-run)
            shift
            inspect_run_cmd "$@"
            ;;
        official-refresh)
            shift
            official_refresh_cmd "$@"
            ;;
        support-check)
            shift
            support_check_cmd "$@"
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
