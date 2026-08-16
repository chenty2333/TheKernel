#!/usr/bin/env bash

# Final evidence assembly for pr-gate.sh. The caller owns errexit policy and
# invokes pr_evidence_finalize from an EXIT trap so failed gates retain a
# diagnostic receipt without ever being labelled PASS.

pr_evidence_log_marker_name=.thekernel-pr-gate-owned

pr_evidence_canonical_path() {
    realpath -m -- "$1"
}

pr_evidence_path_is_within() {
    local path=$1
    local root=$2
    [ "$path" = "$root" ] || [[ "$path" == "$root/"* ]]
}

pr_evidence_validate_log_dir() {
    local candidate
    local requested=$1
    local lexical
    local root
    lexical=$(realpath -ms -- "$requested") || return 1
    candidate=$(pr_evidence_canonical_path "$requested") || return 1
    shift
    [ "$candidate" = "$lexical" ] || {
        printf 'PR gate: log directory contains a symbolic-link component: %s\n' \
            "$requested" >&2
        return 1
    }
    [ "$candidate" != / ] || {
        printf '%s\n' 'PR gate: refusing to use / as a log directory' >&2
        return 1
    }
    for root in "$@"; do
        root=$(realpath -e -- "$root") || return 1
        if pr_evidence_path_is_within "$candidate" "$root"; then
            printf 'PR gate: log directory is inside source repository: %s\n' \
                "$root" >&2
            return 1
        fi
    done
    printf '%s\n' "$candidate"
}

pr_evidence_log_dir_is_available() {
    local directory=$1
    local marker="$directory/$pr_evidence_log_marker_name"

    if [ -e "$directory" ] && [ ! -d "$directory" ]; then
        printf 'PR gate: log path is not a directory: %s\n' "$directory" >&2
        return 1
    fi
    if [ -L "$marker" ] || { [ -e "$marker" ] && [ ! -f "$marker" ]; }; then
        printf 'PR gate: unsafe log-directory ownership marker: %s\n' \
            "$marker" >&2
        return 1
    fi
    if [ -d "$directory" ]; then
        if find "$directory" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
            printf 'PR gate: refusing to reuse non-empty evidence directory: %s\n' \
                "$directory" >&2
            return 1
        fi
    fi
}

pr_evidence_prepare_log_dir() {
    local directory=$1
    local marker="$directory/$pr_evidence_log_marker_name"

    pr_evidence_log_dir_is_available "$directory" || return 1
    mkdir -p -- "$directory"
    (
        set -o noclobber
        printf '%s\n' \
            $'schema\tthekernel-pr-log-dir-v1' \
            $'owner\tpr-gate' >"$marker"
    )
}

pr_evidence_reseal_checksum_census() {
    local evidence_dir=$1
    local checksum_path="$evidence_dir/checksums.sha256"
    local temporary
    local path
    local relative

    [ -d "$evidence_dir" ] && [ ! -L "$evidence_dir" ] || return 1
    temporary=$(mktemp "${TMPDIR:-/tmp}/thekernel-pr-checksums.XXXXXX") \
        || return 1
    if ! (
        cd -- "$evidence_dir" || exit 1
        while IFS= read -r -d '' path; do
            relative=${path#./}
            sha256sum -- "$relative" || exit 1
        done < <(
            find . -type f ! -path ./checksums.sha256 -print0 | LC_ALL=C sort -z
        )
    ) >"$temporary"
    then
        rm -f -- "$temporary"
        return 1
    fi
    mv -- "$temporary" "$checksum_path"
}

pr_evidence_write_gate_envelope() {
    local evidence_dir=$1
    local result=$2
    local child_status=$3
    local origin_result=$4
    local release_qualified=$5
    local reason=$6
    local initial_head=$7
    local initial_ax_head=$8
    local initial_linux_abi_head=$9
    local final_head=${10}
    local final_ax_head=${11}
    local final_linux_abi_head=${12}
    local receipt="$evidence_dir/receipt.tsv"
    local envelope="$evidence_dir/gate-envelope.tsv"
    local temporary

    case "$result" in PASS|FAIL) ;; *) return 1 ;; esac
    [[ "$child_status" =~ ^[0-9]+$ ]] || return 1
    case "$origin_result" in PASS|FAIL) ;; *) return 1 ;; esac
    case "$release_qualified" in YES|NO) ;; *) return 1 ;; esac
    [[ "$reason" =~ ^[A-Za-z0-9._-]+$ ]] || return 1
    [ -s "$receipt" ] && [ ! -L "$receipt" ] || return 1
    [ ! -L "$envelope" ] || return 1
    temporary=$(mktemp "$evidence_dir/.gate-envelope.XXXXXX") || return 1

    if ! {
        printf 'schema\tpr-gate-envelope-v1\n'
        printf 'result\t%s\n' "$result"
        printf 'child_exit_code\t%s\n' "$child_status"
        printf 'origin_source_revalidated\t%s\n' "$origin_result"
        printf 'release_qualified\t%s\n' "$release_qualified"
        printf 'reason\t%s\n' "$reason"
        printf 'inner_receipt_sha256\t%s\n' \
            "$(sha256sum "$receipt" | awk '{print $1}')"
        printf 'origin_initial_head\t%s\n' "$initial_head"
        printf 'origin_initial_ax_head\t%s\n' "$initial_ax_head"
        printf 'origin_initial_linux_abi_head\t%s\n' "$initial_linux_abi_head"
        printf 'origin_final_head\t%s\n' "$final_head"
        printf 'origin_final_ax_head\t%s\n' "$final_ax_head"
        printf 'origin_final_linux_abi_head\t%s\n' "$final_linux_abi_head"
    } >"$temporary"
    then
        rm -f -- "$temporary"
        return 1
    fi
    mv -- "$temporary" "$envelope"
}

pr_evidence_snapshot_repo() {
    local repo=$1
    PR_SNAPSHOT_HEAD=missing
    PR_SNAPSHOT_TREE=missing
    PR_SNAPSHOT_STATE=missing
    if ! git -C "$repo" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        return 1
    fi
    PR_SNAPSHOT_HEAD=$(git -C "$repo" rev-parse HEAD) || return 1
    PR_SNAPSHOT_TREE=$(git -C "$repo" rev-parse 'HEAD^{tree}') || return 1
    if [ -n "$(git -C "$repo" status --porcelain=v1 --untracked-files=all)" ]; then
        PR_SNAPSHOT_STATE=dirty
    else
        PR_SNAPSHOT_STATE=clean
    fi
}

pr_evidence_source_row() {
    local phase=$1
    local label=$2
    local repo=$3
    local expected=$4
    pr_evidence_snapshot_repo "$repo" || true
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$phase" "$label" "$PR_SNAPSHOT_HEAD" "$PR_SNAPSHOT_TREE" \
        "$PR_SNAPSHOT_STATE" "$expected" "sources/$label" \
        >>"$PR_EVIDENCE_SOURCE_SET" \
        || return 1
}

pr_evidence_initialize() {
    PR_EVIDENCE_REPO_ROOT=$1
    PR_EVIDENCE_LOG_DIR=$2
    PR_EVIDENCE_AX_REPO=$3
    PR_EVIDENCE_LINUX_ABI_REPO=$4
    PR_EVIDENCE_BUILD_MODE=$5
    PR_EVIDENCE_AX_EXPECTED=$6
    PR_EVIDENCE_LINUX_ABI_EXPECTED=$7
    PR_EVIDENCE_STARTED_UTC=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    PR_EVIDENCE_DIR="$PR_EVIDENCE_LOG_DIR/evidence"
    PR_EVIDENCE_SOURCE_SET="$PR_EVIDENCE_DIR/source-set.tsv"
    PR_EVIDENCE_ARTIFACTS="$PR_EVIDENCE_DIR/artifacts.tsv"
    PR_EVIDENCE_RECEIPT="$PR_EVIDENCE_DIR/receipt.tsv"
    PR_EVIDENCE_CHECKSUMS="$PR_EVIDENCE_DIR/checksums.sha256"

    [ ! -e "$PR_EVIDENCE_DIR" ] || return 1
    mkdir -p -- "$PR_EVIDENCE_DIR" || return 1
    printf '%s\n' \
        $'schema\tpr-gate-source-set-v2' \
        $'phase\trepository\thead\ttree\tstate\texpected_head\tpath' \
        >"$PR_EVIDENCE_SOURCE_SET" || return 1

    pr_evidence_source_row start TheKernel "$PR_EVIDENCE_REPO_ROOT" - || return 1
    PR_EVIDENCE_START_HEAD=$PR_SNAPSHOT_HEAD
    PR_EVIDENCE_START_TREE=$PR_SNAPSHOT_TREE
    PR_EVIDENCE_START_STATE=$PR_SNAPSHOT_STATE
    pr_evidence_source_row start thekernel-ax \
        "$PR_EVIDENCE_AX_REPO" "$PR_EVIDENCE_AX_EXPECTED" || return 1
    PR_EVIDENCE_START_AX_HEAD=$PR_SNAPSHOT_HEAD
    PR_EVIDENCE_START_AX_TREE=$PR_SNAPSHOT_TREE
    PR_EVIDENCE_START_AX_STATE=$PR_SNAPSHOT_STATE
    pr_evidence_source_row start thekernel-linux-abi \
        "$PR_EVIDENCE_LINUX_ABI_REPO" "$PR_EVIDENCE_LINUX_ABI_EXPECTED" || return 1
    PR_EVIDENCE_START_LINUX_ABI_HEAD=$PR_SNAPSHOT_HEAD
    PR_EVIDENCE_START_LINUX_ABI_TREE=$PR_SNAPSHOT_TREE
    PR_EVIDENCE_START_LINUX_ABI_STATE=$PR_SNAPSHOT_STATE

    [ "$PR_EVIDENCE_START_STATE" = clean ] &&
        [ "$PR_EVIDENCE_START_AX_STATE" = clean ] &&
        [ "$PR_EVIDENCE_START_LINUX_ABI_STATE" = clean ]
}

pr_evidence_copy_file() {
    local source=$1
    local relative=$2
    local destination="$PR_EVIDENCE_DIR/bundle/$relative"
    [ -f "$source" ] && [ ! -L "$source" ] || return 1
    mkdir -p -- "$(dirname -- "$destination")" || return 1
    cp -p -- "$source" "$destination"
}

pr_evidence_require_file() {
    local path=$1
    [ -s "$path" ] || {
        PR_EVIDENCE_ARTIFACTS_COMPLETE=0
        return 1
    }
}

pr_evidence_stage_log_tree() {
    local source
    local relative
    if find "$PR_EVIDENCE_LOG_DIR" \
        -path "$PR_EVIDENCE_DIR" -prune -o \
        ! -type d ! -type f -print -quit | grep -q .
    then
        return 1
    fi
    while IFS= read -r -d '' source; do
        relative=${source#"$PR_EVIDENCE_LOG_DIR"/}
        pr_evidence_copy_file "$source" "logs/$relative" || return 1
    done < <(
        find "$PR_EVIDENCE_LOG_DIR" \
            -path "$PR_EVIDENCE_DIR" -prune -o \
            -type f -print0 | LC_ALL=C sort -z
    )
}

pr_evidence_write_artifact_set() {
    local path
    local relative
    local index=0
    printf '%s\n' \
        $'schema\tpr-gate-artifact-set-v2' \
        $'artifact\tsize_bytes\tsha256\tpath' \
        >"$PR_EVIDENCE_ARTIFACTS" || return 1
    while IFS= read -r -d '' path; do
        relative=${path#"$PR_EVIDENCE_DIR"/}
        index=$((index + 1))
        printf 'file-%04d\t%s\t%s\t%s\n' \
            "$index" "$(stat -c %s "$path")" \
            "$(sha256sum "$path" | awk '{print $1}')" "$relative" \
            >>"$PR_EVIDENCE_ARTIFACTS" || return 1
    done < <(find "$PR_EVIDENCE_DIR/bundle" -type f -print0 | LC_ALL=C sort -z)
    [ "$index" -gt 0 ]
}

pr_evidence_logical_line_count() {
    local log=$1
    local expected=$2
    awk -v expected="$expected" '
        { sub(/\r$/, "", $0); if ($0 == expected) count += 1 }
        END { print count + 0 }
    ' "$log"
}

pr_evidence_logical_prefix_count() {
    local log=$1
    local prefix=$2
    awk -v prefix="$prefix" '
        { sub(/\r$/, "", $0); if (index($0, prefix) == 1) count += 1 }
        END { print count + 0 }
    ' "$log"
}

pr_evidence_console_has_packet_contract() {
    local log=$1
    local marker
    local send_flags_boundary
    send_flags_boundary='THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY '
    send_flags_boundary+='accepted=OOB,MORE,DONTROUTE,EOR,CONFIRM,NOSIGNAL'
    [ -s "$log" ] || return 1
    for marker in \
        THEKERNEL_PACKET_UDP_PRECONDITION_OK \
        THEKERNEL_PACKET_CREATE_OK \
        THEKERNEL_PACKET_RECEIVE_OK \
        THEKERNEL_PACKET_FAULT_OWNERSHIP_OK \
        THEKERNEL_PACKET_SEND_FLAGS_OK \
        THEKERNEL_PACKET_SEND_OK \
        THEKERNEL_PACKET_OPTIONS_OK \
        THEKERNEL_PACKET_OK \
        THEKERNEL_SYSTEM_TEST_PACKET_OK
    do
        [ "$(pr_evidence_logical_line_count "$log" "$marker")" -eq 1 ] \
            || return 1
    done
    [ "$(pr_evidence_logical_prefix_count \
        "$log" 'THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY ')" -eq 1 ] \
        || return 1
    [ "$(pr_evidence_logical_line_count "$log" "$send_flags_boundary")" -eq 1 ] \
        || return 1
    ! grep -Fq -- 'THEKERNEL_PACKET_FAIL' "$log"
}

pr_evidence_verify_final_source_set() {
    pr_evidence_snapshot_repo "$PR_EVIDENCE_REPO_ROOT" || return 1
    [ "$PR_SNAPSHOT_HEAD" = "$PR_EVIDENCE_START_HEAD" ] &&
        [ "$PR_SNAPSHOT_TREE" = "$PR_EVIDENCE_START_TREE" ] &&
        [ "$PR_SNAPSHOT_STATE" = clean ] || return 1
    pr_evidence_snapshot_repo "$PR_EVIDENCE_AX_REPO" || return 1
    [ "$PR_SNAPSHOT_HEAD" = "$PR_EVIDENCE_START_AX_HEAD" ] &&
        [ "$PR_SNAPSHOT_TREE" = "$PR_EVIDENCE_START_AX_TREE" ] &&
        [ "$PR_SNAPSHOT_STATE" = clean ] || return 1
    pr_evidence_snapshot_repo "$PR_EVIDENCE_LINUX_ABI_REPO" || return 1
    [ "$PR_SNAPSHOT_HEAD" = "$PR_EVIDENCE_START_LINUX_ABI_HEAD" ] &&
        [ "$PR_SNAPSHOT_TREE" = "$PR_EVIDENCE_START_LINUX_ABI_TREE" ] &&
        [ "$PR_SNAPSHOT_STATE" = clean ]
}

pr_evidence_verify_artifact_set() {
    local artifact expected_size expected_sha path actual_size actual_sha
    local schema header descriptor
    exec {descriptor}<"$PR_EVIDENCE_ARTIFACTS" || return 1
    IFS= read -r schema <&"$descriptor" || return 1
    IFS= read -r header <&"$descriptor" || return 1
    [ "$schema" = $'schema\tpr-gate-artifact-set-v2' ] || return 1
    [ "$header" = $'artifact\tsize_bytes\tsha256\tpath' ] || return 1
    while IFS=$'\t' read -r artifact expected_size expected_sha path \
        <&"$descriptor"
    do
        [ -n "$artifact" ] || return 1
        case "$path" in
            bundle/*) ;;
            *) return 1 ;;
        esac
        [[ "$path" != *'/../'* ]] || return 1
        actual_path="$PR_EVIDENCE_DIR/$path"
        [ -f "$actual_path" ] || return 1
        actual_size=$(stat -c %s "$actual_path") || return 1
        actual_sha=$(sha256sum "$actual_path" | awk '{print $1}') || return 1
        [ "$actual_size" = "$expected_size" ] || return 1
        [ "$actual_sha" = "$expected_sha" ] || return 1
    done
    exec {descriptor}<&-
}

pr_evidence_status_contract() {
    local status_file="$PR_EVIDENCE_LOG_DIR/status.tsv"
    [ -s "$status_file" ] || return 1
    awk -F '\t' -v build_mode="$PR_EVIDENCE_BUILD_MODE" '
        BEGIN {
            if (build_mode == "source") {
                expected_count = 6
                expected[1] = "clippy-x86_64"
                expected[2] = "release-consumer"
                expected[3] = "release-kernels"
                expected[4] = "release-shell-kernels"
                expected[5] = "boot-shell"
                expected[6] = "system-x86_64"
            } else {
                expected_count = 3
                expected[1] = "clippy-x86_64"
                expected[2] = "boot-shell"
                expected[3] = "system-x86_64"
            }
        }
        NR == 1 {
            if ($0 != "step\tstatus\texit_code\tlog") exit 10
            next
        }
        {
            row += 1
            if (row > expected_count || $1 != expected[row] ||
                $2 != "pass" || $3 != "0" || $4 == "") exit 11
        }
        END {
            if (row != expected_count) exit 12
        }
    ' "$status_file"
}

pr_evidence_revalidate_sources() {
    PR_EVIDENCE_SOURCES_REVALIDATED=1

    pr_evidence_source_row final TheKernel "$PR_EVIDENCE_REPO_ROOT" - \
        || PR_EVIDENCE_SOURCES_REVALIDATED=0
    [ "$PR_SNAPSHOT_HEAD" = "$PR_EVIDENCE_START_HEAD" ] &&
        [ "$PR_SNAPSHOT_TREE" = "$PR_EVIDENCE_START_TREE" ] &&
        [ "$PR_SNAPSHOT_STATE" = clean ] || PR_EVIDENCE_SOURCES_REVALIDATED=0

    pr_evidence_source_row final thekernel-ax \
        "$PR_EVIDENCE_AX_REPO" "$PR_EVIDENCE_AX_EXPECTED" \
        || PR_EVIDENCE_SOURCES_REVALIDATED=0
    [ "$PR_SNAPSHOT_HEAD" = "$PR_EVIDENCE_START_AX_HEAD" ] &&
        [ "$PR_SNAPSHOT_TREE" = "$PR_EVIDENCE_START_AX_TREE" ] &&
        [ "$PR_SNAPSHOT_STATE" = clean ] || PR_EVIDENCE_SOURCES_REVALIDATED=0

    pr_evidence_source_row final thekernel-linux-abi \
        "$PR_EVIDENCE_LINUX_ABI_REPO" "$PR_EVIDENCE_LINUX_ABI_EXPECTED" \
        || PR_EVIDENCE_SOURCES_REVALIDATED=0
    [ "$PR_SNAPSHOT_HEAD" = "$PR_EVIDENCE_START_LINUX_ABI_HEAD" ] &&
        [ "$PR_SNAPSHOT_TREE" = "$PR_EVIDENCE_START_LINUX_ABI_TREE" ] &&
        [ "$PR_SNAPSHOT_STATE" = clean ] || PR_EVIDENCE_SOURCES_REVALIDATED=0
}

pr_evidence_finalize() {
    local command_status=$1
    local result=FAIL
    local release_evidence=NO
    local source_result=FAIL
    local artifact_result=FAIL
    local x86_64_packet_result=FAIL
    local step_status_result=FAIL
    local artifact_hash_result=FAIL
    local effective_status=$command_status
    local receipt_tmp="$PR_EVIDENCE_RECEIPT.tmp"
    local required_path

    pr_evidence_revalidate_sources
    [ "$PR_EVIDENCE_SOURCES_REVALIDATED" -eq 0 ] || source_result=PASS

    PR_EVIDENCE_ARTIFACTS_COMPLETE=1
    [ ! -e "$PR_EVIDENCE_DIR/bundle" ] || return 1
    mkdir -p -- "$PR_EVIDENCE_DIR/bundle/products"

    for required_path in \
        "$PR_EVIDENCE_REPO_ROOT/kernel-x86_64" \
        "$PR_EVIDENCE_REPO_ROOT/.state/shell/kernel-x86_64" \
        "$PR_EVIDENCE_REPO_ROOT/.state/rootfs/rootfs-x86.img" \
        "$PR_EVIDENCE_LOG_DIR/status.tsv" \
        "$PR_EVIDENCE_LOG_DIR/boot/x86_64/qemu.log" \
        "$PR_EVIDENCE_LOG_DIR/boot/x86_64/qemu-runner-receipt.json" \
        "$PR_EVIDENCE_LOG_DIR/system/x86_64/console.log"
    do
        pr_evidence_require_file "$required_path" || true
    done
    if [ "$PR_EVIDENCE_BUILD_MODE" = source ]; then
        pr_evidence_require_file \
            "$PR_EVIDENCE_LOG_DIR/release-consumer/release-set.tsv" || true
        for required_path in \
            release-consumer release-kernels release-shell-kernels \
            boot-shell system-x86_64
        do
            pr_evidence_require_file \
                "$PR_EVIDENCE_LOG_DIR/$required_path.log" || true
        done
    else
        for required_path in boot-shell system-x86_64; do
            pr_evidence_require_file \
                "$PR_EVIDENCE_LOG_DIR/$required_path.log" || true
        done
    fi

    pr_evidence_copy_file "$PR_EVIDENCE_REPO_ROOT/kernel-x86_64" \
        products/kernel-x86_64 || PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    pr_evidence_copy_file "$PR_EVIDENCE_REPO_ROOT/.state/shell/kernel-x86_64" \
        products/shell-kernel-x86_64 || PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    pr_evidence_copy_file "$PR_EVIDENCE_REPO_ROOT/.state/rootfs/rootfs-x86.img" \
        products/rootfs-x86.img || PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    pr_evidence_stage_log_tree || PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    pr_evidence_write_artifact_set || PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    cp -p -- "$PR_EVIDENCE_REPO_ROOT/scripts/ci/verify-pr-gate-evidence.sh" \
        "$PR_EVIDENCE_DIR/verify.sh" || PR_EVIDENCE_ARTIFACTS_COMPLETE=0

    if pr_evidence_console_has_packet_contract \
        "$PR_EVIDENCE_LOG_DIR/system/x86_64/console.log"; then
        x86_64_packet_result=PASS
    else
        PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    fi
    if pr_evidence_status_contract; then
        step_status_result=PASS
    else
        PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    fi
    if [ "$PR_EVIDENCE_ARTIFACTS_COMPLETE" -eq 1 ] &&
        pr_evidence_verify_artifact_set; then
        artifact_hash_result=PASS
    else
        PR_EVIDENCE_ARTIFACTS_COMPLETE=0
    fi
    if ! pr_evidence_verify_final_source_set; then
        PR_EVIDENCE_SOURCES_REVALIDATED=0
        source_result=FAIL
    fi
    [ "$PR_EVIDENCE_ARTIFACTS_COMPLETE" -eq 0 ] || artifact_result=PASS

    if [ "$command_status" -eq 0 ] && [ "$source_result" = PASS ] &&
        [ "$artifact_result" = PASS ]; then
        result=PASS
        effective_status=0
        [ "$PR_EVIDENCE_BUILD_MODE" != source ] || release_evidence=YES
    elif [ "$effective_status" -eq 0 ]; then
        effective_status=1
    fi

    {
        printf 'schema\tpr-gate-receipt-v2\n'
        printf 'result\t%s\n' "$result"
        printf 'command_exit_code\t%s\n' "$command_status"
        printf 'effective_exit_code\t%s\n' "$effective_status"
        printf 'build_mode\t%s\n' "$PR_EVIDENCE_BUILD_MODE"
        printf 'source_execution\tcommit-materialized\n'
        printf 'release_evidence\t%s\n' "$release_evidence"
        printf 'started_utc\t%s\n' "$PR_EVIDENCE_STARTED_UTC"
        printf 'finished_utc\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf 'source_set_revalidated\t%s\n' "$source_result"
        printf 'artifact_set_complete\t%s\n' "$artifact_result"
        printf 'x86_64_packet_markers\t%s\n' "$x86_64_packet_result"
        printf 'step_statuses\t%s\n' "$step_status_result"
        printf 'artifact_hashes_revalidated\t%s\n' "$artifact_hash_result"
        printf 'source_set_sha256\t%s\n' \
            "$(sha256sum "$PR_EVIDENCE_SOURCE_SET" | awk '{print $1}')"
        printf 'artifact_set_sha256\t%s\n' \
            "$(sha256sum "$PR_EVIDENCE_ARTIFACTS" | awk '{print $1}')"
        if [ -f "$PR_EVIDENCE_LOG_DIR/status.tsv" ]; then
            printf 'status_sha256\t%s\n' \
                "$(sha256sum "$PR_EVIDENCE_LOG_DIR/status.tsv" | awk '{print $1}')"
        else
            printf 'status_sha256\tmissing\n'
        fi
    } >"$receipt_tmp" || return 1
    mv -- "$receipt_tmp" "$PR_EVIDENCE_RECEIPT" || return 1

    pr_evidence_reseal_checksum_census "$PR_EVIDENCE_DIR" || return 1

    "$PR_EVIDENCE_DIR/verify.sh" \
        "$PR_EVIDENCE_DIR" >/dev/null || return 1

    printf 'PR evidence candidate sealed: receipt=%s\n' \
        "$PR_EVIDENCE_RECEIPT" >&2
    [ "$effective_status" -eq 0 ]
}
