#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=lib.sh
source "$SCRIPT_DIR/lib.sh"
# shellcheck source=pr-gate-evidence.sh
source "$SCRIPT_DIR/pr-gate-evidence.sh"
# shellcheck source=exact-source-lib.sh
source "$SCRIPT_DIR/exact-source-lib.sh"

default_log_id="$(git -C "$REPO_ROOT" rev-parse --short=12 HEAD)-$(date -u +%Y%m%dT%H%M%SZ)-$$"
default_log_dir="$REPO_ROOT/../.thekernel-ci/pr-$default_log_id"
LOG_DIR=${THEKERNEL_PR_LOG_DIR:-$default_log_dir}
BUILD_TIMEOUT_SECS=${THEKERNEL_CI_BUILD_TIMEOUT_SECS:-3600}
RELEASE_CONSUMER_TIMEOUT_SECS=${THEKERNEL_CI_RELEASE_CONSUMER_TIMEOUT_SECS:-$BUILD_TIMEOUT_SECS}
BOOT_GATE_TIMEOUT_SECS=${THEKERNEL_CI_BOOT_GATE_TIMEOUT_SECS:-900}
SYSTEM_GATE_TIMEOUT_SECS=${THEKERNEL_CI_SYSTEM_TIMEOUT_SECS:-300}
SKIP_BUILD=0
AX_REPO=${THEKERNEL_AX_REPO:-$REPO_ROOT/../thekernel-ax}
LINUX_ABI_REPO=${THEKERNEL_LINUX_ABI_REPO:-$REPO_ROOT/../thekernel-linux-abi}

usage() {
    cat <<'EOF'
Usage: scripts/ci/pr-gate.sh [--log-dir DIR] [--skip-build]

Validates the exact maintained-sibling release artifacts, builds the
release-mode kernel, boot-shell kernel, and repository-built rootfs fixture,
then runs the strict boot-shell and semantic system-test gates.

The normal build path requires THEKERNEL_AX_REF and THEKERNEL_LINUX_ABI_REF to
be the exact 40-hex commits provisioned by CI. --skip-build reuses existing
kernels and skips the release-consumer artifact gate together with compilation.
The verified release set and a self-contained checksum-sealed artifact bundle
are saved below the PR log directory. The directory must be outside all three
source repositories and must be empty; evidence is never overwritten.
EOF
}

while (($#)); do
    case "$1" in
        --log-dir) LOG_DIR=${2:-}; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) ci_die "unknown PR gate argument: $1" ;;
    esac
done

ci_require_positive_int build_timeout "$BUILD_TIMEOUT_SECS"
ci_require_positive_int boot_gate_timeout "$BOOT_GATE_TIMEOUT_SECS"
ci_require_positive_int system_gate_timeout "$SYSTEM_GATE_TIMEOUT_SECS"
case "$LOG_DIR" in
    /*) ;;
    *) LOG_DIR="$REPO_ROOT/$LOG_DIR" ;;
esac
AX_REPO=$(realpath -e -- "$AX_REPO")
LINUX_ABI_REPO=$(realpath -e -- "$LINUX_ABI_REPO")
LOG_DIR=$(pr_evidence_validate_log_dir \
    "$LOG_DIR" "$REPO_ROOT" "$AX_REPO" "$LINUX_ABI_REPO") || exit 2
pr_evidence_log_dir_is_available "$LOG_DIR" || exit 2

AX_REF=${THEKERNEL_AX_REF:-}
LINUX_ABI_REF=${THEKERNEL_LINUX_ABI_REF:-}
BUILD_MODE=source
[ "$SKIP_BUILD" -eq 0 ] || BUILD_MODE=reuse

if [ "${THEKERNEL_PR_GATE_MATERIALIZED:-0}" != 1 ]; then
    exact_source_require_clean_repo TheKernel "$REPO_ROOT" || exit 1
    exact_source_require_clean_repo thekernel-ax "$AX_REPO" || exit 1
    exact_source_require_clean_repo thekernel-linux-abi "$LINUX_ABI_REPO" || exit 1

    source_head=$(git -C "$REPO_ROOT" rev-parse HEAD)
    ax_head=$(git -C "$AX_REPO" rev-parse HEAD)
    linux_abi_head=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD)
    if [ "$SKIP_BUILD" -eq 0 ]; then
        [[ "$AX_REF" =~ ^[0-9a-f]{40}$ ]] \
            || ci_die 'THEKERNEL_AX_REF must be the provisioned exact 40-hex commit'
        [[ "$LINUX_ABI_REF" =~ ^[0-9a-f]{40}$ ]] \
            || ci_die 'THEKERNEL_LINUX_ABI_REF must be the provisioned exact 40-hex commit'
        [ "$ax_head" = "$AX_REF" ] \
            || ci_die 'thekernel-ax checkout does not match THEKERNEL_AX_REF'
        [ "$linux_abi_head" = "$LINUX_ABI_REF" ] \
            || ci_die 'thekernel-linux-abi checkout does not match THEKERNEL_LINUX_ABI_REF'
    fi

    materialization=$(mktemp -d "${TMPDIR:-/tmp}/thekernel-pr-exact.XXXXXX")
    cleanup_materialization() {
        rm -rf -- "$materialization"
    }
    trap cleanup_materialization EXIT
    exact_source_materialize_set "$materialization/sources" \
        "$REPO_ROOT" "$source_head" "$AX_REPO" "$ax_head" \
        "$LINUX_ABI_REPO" "$linux_abi_head"

    materialized_repo="$materialization/sources/TheKernel"
    if [ "$SKIP_BUILD" -eq 1 ]; then
        for reuse_path in \
            kernel-x86_64 \
            .state/shell/kernel-x86_64 \
            .state/rootfs/rootfs-x86.img
        do
            [ -s "$REPO_ROOT/$reuse_path" ] \
                || ci_die "missing $reuse_path for --skip-build"
            mkdir -p -- "$(dirname -- "$materialized_repo/$reuse_path")"
            cp -p -- "$REPO_ROOT/$reuse_path" "$materialized_repo/$reuse_path"
        done
    fi

    child_args=(--log-dir "$LOG_DIR")
    [ "$SKIP_BUILD" -eq 0 ] || child_args+=(--skip-build)
    set +e
    THEKERNEL_PR_GATE_MATERIALIZED=1 \
        THEKERNEL_AX_REPO="$materialization/sources/thekernel-ax" \
        THEKERNEL_LINUX_ABI_REPO="$materialization/sources/thekernel-linux-abi" \
        THEKERNEL_EXACT_SOURCE_RECEIPT="$materialization/sources/source-set.tsv" \
        THEKERNEL_AX_REF="$ax_head" \
        THEKERNEL_LINUX_ABI_REF="$linux_abi_head" \
        "$materialized_repo/scripts/ci/pr-gate.sh" "${child_args[@]}"
    child_status=$?
    set -e
    terminal_status=$child_status
    origin_result=PASS
    exact_source_require_clean_repo TheKernel "$REPO_ROOT" \
        || origin_result=FAIL
    exact_source_require_clean_repo thekernel-ax "$AX_REPO" \
        || origin_result=FAIL
    exact_source_require_clean_repo thekernel-linux-abi "$LINUX_ABI_REPO" \
        || origin_result=FAIL
    final_source_head=$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null \
        || printf '%s' missing)
    final_ax_head=$(git -C "$AX_REPO" rev-parse HEAD 2>/dev/null \
        || printf '%s' missing)
    final_linux_abi_head=$(git -C "$LINUX_ABI_REPO" rev-parse HEAD 2>/dev/null \
        || printf '%s' missing)
    [ "$final_source_head" = "$source_head" ] || origin_result=FAIL
    [ "$final_ax_head" = "$ax_head" ] || origin_result=FAIL
    [ "$final_linux_abi_head" = "$linux_abi_head" ] || origin_result=FAIL

    envelope_result=FAIL
    release_qualified=NO
    terminal_reason=child-gate-failed
    if [ "$origin_result" = FAIL ]; then
        terminal_reason=origin-source-changed
        [ "$terminal_status" -ne 0 ] || terminal_status=1
    elif [ "$child_status" -eq 0 ]; then
        envelope_result=PASS
        if [ "$SKIP_BUILD" -eq 0 ]; then
            release_qualified=YES
            terminal_reason=release-qualified
        else
            terminal_reason=reuse-non-release
        fi
    fi

    evidence_dir="$LOG_DIR/evidence"
    evidence_status=0
    set +e
    pr_evidence_write_gate_envelope \
        "$evidence_dir" "$envelope_result" "$child_status" "$origin_result" \
        "$release_qualified" "$terminal_reason" \
        "$source_head" "$ax_head" "$linux_abi_head" \
        "$final_source_head" "$final_ax_head" "$final_linux_abi_head"
    evidence_status=$?
    if [ "$evidence_status" -eq 0 ]; then
        pr_evidence_reseal_checksum_census "$evidence_dir"
        evidence_status=$?
    fi
    if [ "$evidence_status" -eq 0 ]; then
        if [ "$release_qualified" = YES ]; then
            "$evidence_dir/verify.sh" --require-release-pass "$evidence_dir"
        else
            "$evidence_dir/verify.sh" "$evidence_dir"
        fi
        evidence_status=$?
    fi
    if [ "$evidence_status" -ne 0 ]; then
        envelope_result=FAIL
        release_qualified=NO
        terminal_reason=evidence-verification-failed
        [ "$terminal_status" -ne 0 ] || terminal_status=1
        pr_evidence_write_gate_envelope \
            "$evidence_dir" FAIL "$child_status" "$origin_result" NO \
            "$terminal_reason" \
            "$source_head" "$ax_head" "$linux_abi_head" \
            "$final_source_head" "$final_ax_head" "$final_linux_abi_head" \
            && pr_evidence_reseal_checksum_census "$evidence_dir" \
            && "$evidence_dir/verify.sh" "$evidence_dir" >/dev/null
    fi
    set -e
    trap - EXIT
    cleanup_materialization
    if [ "$terminal_status" -eq 0 ]; then
        if [ "$SKIP_BUILD" -eq 0 ]; then
            printf 'PR gate: PASS\n'
        else
            printf 'PR gate: NON_RELEASE_OK (reused artifacts)\n'
        fi
        printf 'logs: %s\n' "$LOG_DIR"
    else
        printf 'PR gate: FAIL reason=%s exit=%s\n' \
            "$terminal_reason" "$terminal_status" >&2
    fi
    exit "$terminal_status"
fi

cd "$REPO_ROOT"
exact_source_verify_materialization \
    "${THEKERNEL_EXACT_SOURCE_RECEIPT:-}" \
    "$REPO_ROOT" "$AX_REPO" "$LINUX_ABI_REPO" \
    || ci_die 'PR gate internal execution requires a verified materialized source set'
pr_evidence_prepare_log_dir "$LOG_DIR" || exit 2
export CI_LOG_DIR="$LOG_DIR"
ci_prepare_log_dir "$CI_LOG_DIR"

pr_gate_on_exit() {
    local command_status=$?
    local evidence_status
    trap - EXIT
    set +e
    pr_evidence_finalize "$command_status"
    evidence_status=$?
    if [ "$command_status" -eq 0 ] && [ "$evidence_status" -ne 0 ]; then
        command_status=1
    fi
    exit "$command_status"
}

trap pr_gate_on_exit EXIT
if ! pr_evidence_initialize \
    "$REPO_ROOT" "$LOG_DIR" "$AX_REPO" "$LINUX_ABI_REPO" \
    "$BUILD_MODE" "${AX_REF:--}" "${LINUX_ABI_REF:--}"
then
    ci_die 'PR gate requires clean TheKernel and maintained-sibling source worktrees'
fi

# The per-commit gate lints the host and x86_64 profiles before the build.
ci_run_step clippy-x86_64 "$BUILD_TIMEOUT_SECS" \
    "$SCRIPT_DIR/clippy-gate.sh" --profile x86_64

if [ "$SKIP_BUILD" -eq 0 ]; then
    ci_require_positive_int release_consumer_timeout "$RELEASE_CONSUMER_TIMEOUT_SECS"
    RELEASE_SET="$LOG_DIR/release-consumer/release-set.tsv"
    [ ! -e "$RELEASE_SET" ] || ci_die 'release-set path unexpectedly exists'
    [[ "$AX_REF" =~ ^[0-9a-f]{40}$ ]] \
        || ci_die 'THEKERNEL_AX_REF must be the provisioned exact 40-hex commit'
    [[ "$LINUX_ABI_REF" =~ ^[0-9a-f]{40}$ ]] \
        || ci_die 'THEKERNEL_LINUX_ABI_REF must be the provisioned exact 40-hex commit'
    [ "$PR_EVIDENCE_START_AX_HEAD" = "$AX_REF" ] \
        || ci_die 'thekernel-ax checkout does not match THEKERNEL_AX_REF'
    [ "$PR_EVIDENCE_START_LINUX_ABI_HEAD" = "$LINUX_ABI_REF" ] \
        || ci_die 'thekernel-linux-abi checkout does not match THEKERNEL_LINUX_ABI_REF'

    ci_run_step release-consumer "$RELEASE_CONSUMER_TIMEOUT_SECS" \
        "$SCRIPT_DIR/release-consumer-gate.sh" \
        --arch x86_64 \
        --ax-head "$AX_REF" \
        --linux-abi-head "$LINUX_ABI_REF" \
        --output-release-set "$RELEASE_SET"
    ci_run_step release-kernels "$BUILD_TIMEOUT_SECS" make kernels
    ci_run_step release-shell-kernels "$BUILD_TIMEOUT_SECS" \
        make kernel-x86_64-shell rootfs-x86
else
    [ -s kernel-x86_64 ] || ci_die 'missing kernel-x86_64 for --skip-build'
    [ -s .state/shell/kernel-x86_64 ] \
        || ci_die 'missing x86_64 shell kernel for --skip-build'
    [ -s .state/rootfs/rootfs-x86.img ] \
        || ci_die 'missing x86_64 rootfs for --skip-build'
fi

ci_run_step boot-shell "$BOOT_GATE_TIMEOUT_SECS" \
    "$SCRIPT_DIR/boot-shell-gate.sh" \
    --arch x86_64 --skip-build --log-dir "$LOG_DIR/boot"

ci_run_step system-x86_64 "$((SYSTEM_GATE_TIMEOUT_SECS + 90))" \
    "$REPO_ROOT/scripts/system-test.sh" \
    --arch x86_64 --skip-build --timeout "$SYSTEM_GATE_TIMEOUT_SECS" \
    --workdir "$LOG_DIR/system/x86_64"
trap - EXIT
if ! pr_evidence_finalize 0; then
    printf 'PR gate: FAIL (final evidence validation)\n' >&2
    exit 1
fi
printf 'PR gate internal: evidence sealed\n'
