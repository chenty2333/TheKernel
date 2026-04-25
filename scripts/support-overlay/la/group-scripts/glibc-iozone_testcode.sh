#!/bin/sh

IOZONE_BIN=/glibc/iozone

if [ -n "${OSCOMP_IOZONE_CHDIR:-}" ]; then
    cd "$OSCOMP_IOZONE_CHDIR" || exit 1
fi

echo "#### OS COMP TEST GROUP START iozone-glibc ####"

run_iozone_direct() {
    no_unlink="$1"
    shift
    if [ "$no_unlink" = "1" ]; then
        LD_LIBRARY_PATH=/glibc/lib:/lib "$IOZONE_BIN" -w "$@"
    else
        LD_LIBRARY_PATH=/glibc/lib:/lib "$IOZONE_BIN" "$@"
    fi
}

iozone_case_selected() {
    case_name="$1"
    case_filter="${OSCOMP_IOZONE_CASE_FILTER:-}"
    case "$case_filter" in
        "")
            return 0
            ;;
    esac

    normalized_filter="$(printf '%s' "$case_filter" | tr ',' ' ')"
    for token in $normalized_filter; do
        [ "$token" = "$case_name" ] && return 0
    done
    return 1
}

run_iozone_case() {
    case_name="$1"
    case_heading="$2"
    shift 2

    iozone_case_selected "$case_name" || return 0
    echo "$case_heading"
    no_unlink="${OSCOMP_IOZONE_NO_UNLINK:-1}"
    run_iozone_direct "$no_unlink" "$@"
    return $?
}

skip_auto_mode=0
case "${OSCOMP_IOZONE_AUTO_MODE:-}" in
    1|y|Y|yes|YES|true|TRUE|on|ON)
        skip_auto_mode=0
        ;;
    0|n|N|no|NO|false|FALSE|off|OFF)
        skip_auto_mode=1
        ;;
    *)
        skip_auto_mode=1
        ;;
esac

if [ "$skip_auto_mode" -eq 0 ]; then
    run_iozone_case auto "iozone automatic measurements" -a -r 1k -s 4m || exit $?
fi
run_iozone_case write-read "iozone throughput write/read measurements" -t 4 -i 0 -i 1 -r 1k -s 1m || exit $?
run_iozone_case random-read "iozone throughput random-read measurements" -t 4 -i 0 -i 2 -r 1k -s 1m || exit $?
run_iozone_case read-backwards "iozone throughput read-backwards measurements" -t 4 -i 0 -i 3 -r 1k -s 1m || exit $?
run_iozone_case stride-read "iozone throughput stride-read measurements" -t 4 -i 0 -i 5 -r 1k -s 1m || exit $?
run_iozone_case fwrite-fread "iozone throughput fwrite/fread measurements" -t 4 -i 6 -i 7 -r 1k -s 1m || exit $?
run_iozone_case pwrite-pread "iozone throughput pwrite/pread measurements" -t 4 -i 9 -i 10 -r 1k -s 1m || exit $?
run_iozone_case pwritev-preadv "iozone throughtput pwritev/preadv measurements" -t 4 -i 11 -i 12 -r 1k -s 1m || exit $?

echo "#### OS COMP TEST GROUP END iozone-glibc ####"
