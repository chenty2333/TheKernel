#!/usr/bin/env bash

# Shared helpers for local and hosted CI gates. Callers are expected to enable
# `set -euo pipefail` before sourcing this file.

ci_die() {
    printf 'ci: error: %s\n' "$*" >&2
    exit 2
}

ci_require_command() {
    command -v "$1" >/dev/null 2>&1 || ci_die "required command not found: $1"
}

ci_require_positive_int() {
    local name=$1
    local value=$2
    case "$value" in
        ''|*[!0-9]*) ci_die "$name must be a positive integer: $value" ;;
    esac
    [ "$value" -gt 0 ] || ci_die "$name must be greater than zero: $value"
}

ci_require_nonnegative_number() {
    local name=$1
    local value=$2
    case "$value" in
        ''|*[!0-9.]*) ci_die "$name must be a non-negative number: $value" ;;
    esac
    [[ "$value" =~ ^[0-9]+([.][0-9]+)?$ ]] \
        || ci_die "$name must be a non-negative number: $value"
}

ci_prepare_log_dir() {
    local directory=$1
    mkdir -p "$directory"
    rm -f "$directory/status.tsv"
    printf 'step\tstatus\texit_code\tlog\n' >"$directory/status.tsv"
}

ci_path_is_within() {
    local path=$1
    local root=$2
    [ "$path" = "$root" ] || [[ "$path" == "$root/"* ]]
}

ci_validate_owned_run_path() {
    local owner=$1
    local directory=$2
    local repo_root=$3
    local allowed_state_root=$4
    local lexical
    local resolved

    [[ "$owner" =~ ^[A-Za-z0-9._-]+$ ]] \
        || ci_die "unsafe run-directory owner: $owner"
    ci_require_command find
    ci_require_command realpath

    repo_root=$(realpath -e -- "$repo_root") \
        || ci_die "cannot resolve repository root: $repo_root"
    allowed_state_root=$(realpath -m -- "$allowed_state_root") \
        || ci_die "cannot resolve allowed state root: $allowed_state_root"
    lexical=$(realpath -ms -- "$directory") \
        || ci_die "cannot normalize run directory: $directory"
    resolved=$(realpath -m -- "$directory") \
        || ci_die "cannot resolve run directory: $directory"

    [ "$lexical" = "$resolved" ] \
        || ci_die "run directory contains a symbolic-link component: $directory"
    [ "$resolved" != / ] || ci_die 'run directory must not be the filesystem root'
    if ci_path_is_within "$repo_root" "$resolved"; then
        ci_die "run directory must not contain the source repository: $resolved"
    fi
    if ci_path_is_within "$resolved" "$repo_root" &&
        ! ci_path_is_within "$resolved" "$allowed_state_root"
    then
        ci_die "run directory is inside source outside the state root: $resolved"
    fi
    printf '%s\n' "$resolved"
}

# Creates one fresh, marker-owned run directory without deleting prior data.
# Paths below a repository are accepted only beneath the caller's designated
# ignored state root. Existing symlink components and non-empty reuse are
# rejected before any gate writes artifacts.
ci_prepare_owned_run_dir() {
    local owner=$1
    local directory=$2
    local repo_root=$3
    local allowed_state_root=$4
    local resolved
    local physical
    local marker
    local entry

    resolved=$(ci_validate_owned_run_path \
        "$owner" "$directory" "$repo_root" "$allowed_state_root") || return
    [ ! -L "$resolved" ] || ci_die "run directory must not be a symlink: $resolved"
    if [ -e "$resolved" ] && [ ! -d "$resolved" ]; then
        ci_die "run path is not a directory: $resolved"
    fi
    if [ -d "$resolved" ] &&
        find "$resolved" -mindepth 1 -maxdepth 1 -print -quit | grep -q .
    then
        ci_die "refusing to reuse non-empty run directory: $resolved"
    fi

    mkdir -p -- "$resolved"
    physical=$(realpath -e -- "$resolved") \
        || ci_die "cannot resolve created run directory: $resolved"
    [ "$physical" = "$resolved" ] \
        || ci_die "run directory changed through a symbolic link: $directory"
    marker="$physical/.thekernel-ci-owned-run"
    [ ! -e "$marker" ] && [ ! -L "$marker" ] \
        || ci_die "run-directory marker already exists: $marker"
    if ! (
        set -o noclobber
        printf '%s\n' \
            $'schema\tthekernel-ci-owned-run-v1' \
            "owner"$'\t'"$owner" >"$marker"
    ); then
        ci_die "cannot claim run-directory marker: $marker"
    fi
    [ -f "$marker" ] && [ ! -L "$marker" ] \
        || ci_die "unsafe run-directory marker: $marker"
    while IFS= read -r -d '' entry; do
        [ "$entry" = "$marker" ] \
            || ci_die "run directory changed during ownership claim: $physical"
    done < <(find "$physical" -mindepth 1 -maxdepth 1 -print0)

    printf '%s\n' "$physical"
}

ci_run_step() {
    if [ "$#" -lt 3 ]; then
        ci_die 'ci_run_step requires NAME TIMEOUT COMMAND [ARGS...]'
    fi

    local name=$1
    local timeout_secs=$2
    shift 2

    [[ "$name" =~ ^[A-Za-z0-9._-]+$ ]] || ci_die "unsafe step name: $name"
    ci_require_positive_int timeout_secs "$timeout_secs"
    ci_require_command timeout
    ci_require_command tee

    local log_dir=${CI_LOG_DIR:-.state/ci/local}
    local log_path="$log_dir/$name.log"
    local status_file="$log_dir/status.tsv"
    mkdir -p "$log_dir"
    if [ ! -f "$status_file" ]; then
        printf 'step\tstatus\texit_code\tlog\n' >"$status_file"
    fi

    printf '[ci] START %s (timeout=%ss)\n' "$name" "$timeout_secs" | tee "$log_path"

    local had_errexit=0
    [[ $- == *e* ]] && had_errexit=1
    set +e
    timeout --signal=TERM --kill-after=15s "${timeout_secs}s" "$@" 2>&1 \
        | tee -a "$log_path"
    local -a pipeline_status=("${PIPESTATUS[@]}")
    [ "$had_errexit" -eq 0 ] || set -e

    local command_status=${pipeline_status[0]}
    local tee_status=${pipeline_status[1]}
    local status=$command_status
    if [ "$status" -eq 0 ] && [ "$tee_status" -ne 0 ]; then
        status=$tee_status
    fi

    if [ "$status" -eq 0 ]; then
        printf '[ci] PASS %s\n' "$name" | tee -a "$log_path"
        printf '%s\tpass\t0\t%s\n' "$name" "$log_path" >>"$status_file"
    elif [ "$status" -eq 124 ]; then
        printf '[ci] TIMEOUT %s after %ss\n' "$name" "$timeout_secs" | tee -a "$log_path" >&2
        printf '%s\ttimeout\t124\t%s\n' "$name" "$log_path" >>"$status_file"
    else
        printf '[ci] FAIL %s (exit=%s)\n' "$name" "$status" | tee -a "$log_path" >&2
        printf '%s\tfail\t%s\t%s\n' "$name" "$status" "$log_path" >>"$status_file"
    fi
    return "$status"
}
