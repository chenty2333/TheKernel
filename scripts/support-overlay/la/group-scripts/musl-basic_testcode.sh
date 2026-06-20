#!/bin/sh

ROOT=/musl

basic_case_selected() {
    case_name="$1"
    case_filter="${OSCOMP_BASIC_CASE_FILTER:-}"
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

run_case() {
    case_name="$1"
    basic_case_selected "$case_name" || return 0
    echo "Testing $case_name :"
    "./$case_name"
}

echo "#### OS COMP TEST GROUP START basic ####"

cd "$ROOT/basic" || exit 1

for case_name in \
    brk \
    chdir \
    clone \
    close \
    dup2 \
    dup \
    execve \
    exit \
    fork \
    fstat \
    getcwd \
    getdents \
    getpid \
    getppid \
    gettimeofday \
    mkdir_ \
    mmap \
    mount \
    munmap \
    openat \
    open \
    pipe \
    read \
    sleep \
    times \
    umount \
    uname \
    unlink \
    wait \
    waitpid \
    write \
    yield
do
    run_case "$case_name" || exit $?
done

echo "#### OS COMP TEST GROUP END basic ####"
