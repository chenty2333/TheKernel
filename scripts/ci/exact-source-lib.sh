#!/usr/bin/env bash

# Helpers for gates which must execute committed source rather than a mutable
# developer worktree. Callers own errexit policy and temporary-directory
# cleanup.

exact_source_require_clean_repo() {
    local label=$1
    local repo=$2
    local dirty

    git -C "$repo" rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
        printf 'exact-source: %s is not a Git worktree: %s\n' \
            "$label" "$repo" >&2
        return 1
    }
    dirty=$(git -C "$repo" status --porcelain=v1 --untracked-files=all) || return 1
    if [ -n "$dirty" ]; then
        printf 'exact-source: dirty %s source: %s\n%s\n' \
            "$label" "$repo" "$dirty" >&2
        return 1
    fi
}

exact_source_clone_commit() {
    local label=$1
    local source_repo=$2
    local commit=$3
    local destination=$4
    local actual_head

    [[ "$commit" =~ ^[0-9a-f]{40}$ ]] || {
        printf 'exact-source: invalid %s commit: %s\n' "$label" "$commit" >&2
        return 1
    }
    git clone --quiet --no-local --no-checkout -- "$source_repo" "$destination" \
        || return 1
    git -C "$destination" checkout --quiet --detach "$commit" || return 1
    actual_head=$(git -C "$destination" rev-parse HEAD) || return 1
    [ "$actual_head" = "$commit" ] || return 1
    exact_source_require_clean_repo "$label materialization" "$destination"
}

exact_source_materialize_set() {
    local materialization_root=$1
    local primary_repo=$2
    local primary_head=$3
    local ax_repo=$4
    local ax_head=$5
    local linux_abi_repo=$6
    local linux_abi_head=$7
    local primary_tree
    local ax_tree
    local linux_abi_tree

    mkdir -p -- "$materialization_root"
    exact_source_clone_commit TheKernel "$primary_repo" "$primary_head" \
        "$materialization_root/TheKernel" || return 1
    exact_source_clone_commit thekernel-ax "$ax_repo" "$ax_head" \
        "$materialization_root/thekernel-ax" || return 1
    exact_source_clone_commit thekernel-linux-abi \
        "$linux_abi_repo" "$linux_abi_head" \
        "$materialization_root/thekernel-linux-abi" || return 1

    primary_tree=$(git -C "$materialization_root/TheKernel" rev-parse HEAD^{tree})
    ax_tree=$(git -C "$materialization_root/thekernel-ax" rev-parse HEAD^{tree})
    linux_abi_tree=$(
        git -C "$materialization_root/thekernel-linux-abi" rev-parse HEAD^{tree}
    )

    {
        printf 'schema\tcommit-materialized-source-set-v1\n'
        printf 'repository\thead\ttree\tpath\n'
        printf 'TheKernel\t%s\t%s\t%s\n' \
            "$primary_head" "$primary_tree" "$materialization_root/TheKernel"
        printf 'thekernel-ax\t%s\t%s\t%s\n' \
            "$ax_head" "$ax_tree" "$materialization_root/thekernel-ax"
        printf 'thekernel-linux-abi\t%s\t%s\t%s\n' \
            "$linux_abi_head" "$linux_abi_tree" \
            "$materialization_root/thekernel-linux-abi"
    } >"$materialization_root/source-set.tsv"
}

exact_source_verify_materialization() {
    local receipt=$1
    local primary_repo=$2
    local ax_repo=$3
    local linux_abi_repo=$4
    local expected_label
    local expected_repo
    local schema
    local header
    local label
    local head
    local tree
    local path
    local extra
    local index=0
    local descriptor
    local -a labels=(TheKernel thekernel-ax thekernel-linux-abi)
    local -a repos=("$primary_repo" "$ax_repo" "$linux_abi_repo")

    exec {descriptor}<"$receipt" || return 1
    IFS= read -r schema <&"$descriptor" || return 1
    IFS= read -r header <&"$descriptor" || return 1
    [ "$schema" = $'schema\tcommit-materialized-source-set-v1' ] || return 1
    [ "$header" = $'repository\thead\ttree\tpath' ] || return 1
    while IFS=$'\t' read -r label head tree path extra <&"$descriptor"; do
        [ "$index" -lt 3 ] && [ -z "${extra:-}" ] || return 1
        expected_label=${labels[$index]}
        expected_repo=$(realpath -e -- "${repos[$index]}") || return 1
        [ "$label" = "$expected_label" ] || return 1
        [ "$(realpath -e -- "$path")" = "$expected_repo" ] || return 1
        [ "$(git -C "$expected_repo" rev-parse HEAD)" = "$head" ] || return 1
        [ "$(git -C "$expected_repo" rev-parse HEAD^{tree})" = "$tree" ] \
            || return 1
        exact_source_require_clean_repo "$label materialization" "$expected_repo" \
            || return 1
        index=$((index + 1))
    done
    exec {descriptor}<&-
    [ "$index" -eq 3 ]
}
