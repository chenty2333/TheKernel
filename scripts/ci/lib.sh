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

ci_path_is_within() {
    local path=$1
    local root=$2
    [ "$path" = "$root" ] || [[ "$path" == "$root/"* ]]
}

ci_validate_run_path() {
    local directory=$1
    local repo_root=$2
    local allowed_state_root=$3
    local lexical
    local resolved

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

# Creates one fresh run directory without deleting prior data.
# Paths below a repository are accepted only beneath the caller's designated
# ignored state root. Existing symlink components and non-empty reuse are
# rejected before any gate writes artifacts.
ci_prepare_run_dir() {
    local directory=$1
    local repo_root=$2
    local allowed_state_root=$3
    local resolved
    local physical

    resolved=$(ci_validate_run_path \
        "$directory" "$repo_root" "$allowed_state_root") || return
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

    printf '%s\n' "$physical"
}
