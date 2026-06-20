#!/bin/sh

ROOT="${OSCOMP_LIBCTEST_ROOT:-/musl}"
CASE_FILTER="${OSCOMP_LIBCTEST_CASE_FILTER:-}"
ENTRY_FILTER="${OSCOMP_LIBCTEST_ENTRY_FILTER:-}"
PHASE_FILTER="${OSCOMP_LIBCTEST_PHASE_FILTER:-}"

filter_tokens_match() {
    filter_value="$1"
    candidate="$2"

    [ -n "$filter_value" ] || return 0
    normalized_filter="$(printf '%s' "$filter_value" | tr ',' ' ')"
    for token in $normalized_filter; do
        [ "$token" = "$candidate" ] && return 0
    done
    return 1
}

libctest_phase_for_entry() {
    case "$1" in
        *static*)
            printf '%s\n' static
            ;;
        *dynamic*)
            printf '%s\n' dynamic
            ;;
        *)
            printf '%s\n' unknown
            ;;
    esac
}

libctest_case_selected() {
    entry="$1"
    case_name="$2"
    phase="$(libctest_phase_for_entry "$entry")"

    filter_tokens_match "$PHASE_FILTER" "$phase" || return 1
    filter_tokens_match "$ENTRY_FILTER" "$entry" || return 1
    filter_tokens_match "$CASE_FILTER" "$case_name" || return 1
    return 0
}

run_filtered_script() {
    script="$1"
    matched=0
    last_status=0

    while IFS= read -r line || [ -n "$line" ]; do
        set -- $line
        [ "$#" -ge 4 ] || continue
        [ "$1" = "./runtest.exe" ] || continue
        [ "$2" = "-w" ] || continue

        entry="$3"
        case_name="$4"
        libctest_case_selected "$entry" "$case_name" || continue
        matched=1
        ./runtest.exe -w "$entry" "$case_name" </dev/null
        last_status=$?
    done < "$script"

    [ "$matched" -eq 1 ] || echo "libctest filter selected no cases from $script"
    return "$last_status"
}

echo "#### OS COMP TEST GROUP START libctest ####"

cd "$ROOT" || exit 1

if [ -z "$CASE_FILTER" ] && [ -z "$ENTRY_FILTER" ] && [ -z "$PHASE_FILTER" ]; then
    ./run-static.sh
    ./run-dynamic.sh
else
    run_filtered_script ./run-static.sh
    run_filtered_script ./run-dynamic.sh
fi

echo "#### OS COMP TEST GROUP END libctest ####"
