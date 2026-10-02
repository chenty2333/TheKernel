#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
DEV_ENV_DIR="$REPO_ROOT/dev-env"

usage() {
    cat <<'EOF'
Usage:
  scripts/dev-shell.sh [--build]
  scripts/dev-shell.sh [--build] --guest-shell [RUN_ARGS...]
  scripts/dev-shell.sh [--build] -- COMMAND [ARGS...]

Options:
  --build                      Force a rebuild of the default local image

Environment:
  THEKERNEL_DEV_IMAGE        Docker image tag or digest (default: thekernel-dev:local)
  THEKERNEL_ROOTLESS_PODMAN  Set to 1 for a rootless Podman Docker API socket;
                             maps the host caller to container root for bind writes
  THEKERNEL_DEBIAN_MIRROR    Temporary Debian package mirror for local image builds
  THEKERNEL_DEBIAN_SECURITY_MIRROR
                             Temporary Debian security mirror for local image builds
EOF
}

# Host absolute paths are not mounted at the same location inside this shell.
# The container uses its persistent /home/thekernel cache; reject an override
# rather than silently writing to an unrelated or ephemeral container path.
if [[ -n "${THEKERNEL_STATE_DIR:-}" ]]; then
    printf '%s\n' 'dev-shell: THEKERNEL_STATE_DIR is a host-only override; unset it to use the persistent container home cache' >&2
    exit 2
fi

force_build=0
if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi

if [[ "${1:-}" == "--build" ]]; then
    force_build=1
    shift
fi

if [[ "${1:-}" == "--service" ]]; then
    printf '%s\n' 'dev-shell: --service was removed; TheKernel has one development service' >&2
    exit 2
fi

if [[ "${1:-}" == "--guest-shell" ]]; then
    shift
    set -- ./tools/thekernel.py run --profile shell --interactive "$@"
elif [[ $# -gt 0 && "$1" == "--" ]]; then
    shift
fi

if [[ $# -eq 0 ]]; then
    set -- bash
fi

export THEKERNEL_DEV_IMAGE="${THEKERNEL_DEV_IMAGE:-thekernel-dev:local}"
export LOCAL_UID="$(id -u)"
export LOCAL_GID="$(id -g)"
export THEKERNEL_USERNS_MODE=""
case "${THEKERNEL_ROOTLESS_PODMAN:-0}" in
    0) ;;
    1)
        # The existing entrypoint first creates /workspace/.state, then gosu's
        # to LOCAL_UID. Map both operations to the unprivileged host caller;
        # container root here grants no host root privileges.
        export THEKERNEL_USERNS_MODE="keep-id:uid=0,gid=0"
        export LOCAL_UID=0 LOCAL_GID=0
        ;;
    *) printf '%s\n' 'THEKERNEL_ROOTLESS_PODMAN must be 0 or 1' >&2; exit 2 ;;
esac

run_args=(run --rm --remove-orphans)
if [[ ! -t 0 || ! -t 1 ]]; then
    run_args+=(-T)
fi

# QEMU stays inside the same ephemeral development container as the build.
# Pass through only the device nodes that are already present on the host; do
# not use host networking or privileged mode.  A CI runner without KVM simply
# uses TCG, while a local KVM host retains the fast path.
compose_files=(-f "$DEV_ENV_DIR/compose.yaml")
device_override=
cleanup_override() {
    [[ -z "$device_override" ]] || rm -f "$device_override"
}
trap cleanup_override EXIT

# `docker compose run` has no --device option. Generate a tiny, throw-away
# override instead of making the checked-in service depend on a host device
# that GitHub runners or ordinary CI machines may not have.
if [[ -e /dev/kvm || -e /dev/dri/card0 || -e /dev/dri/renderD128 ]]; then
    cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}"
    mkdir -p "$cache_dir"
    device_override=$(mktemp "$cache_dir/thekernel-compose.XXXXXX.yaml")
    {
        printf '%s\n' 'services:' '  dev:' '    devices:'
        for device in /dev/kvm /dev/dri/card* /dev/dri/renderD*; do
            [[ -e "$device" ]] || continue
            printf '      - %s:%s\n' "$device" "$device"
        done
    } > "$device_override"
    compose_files+=(-f "$device_override")
fi

# The desktop starts its own guest PulseAudio server, while QEMU's host-side
# audio backend needs the host Pulse socket. Pass that socket and its cookie
# only when they exist; headless CI remains unchanged.
pulse_socket=
if [[ "${PULSE_SERVER:-}" == unix:* ]]; then
    pulse_socket=${PULSE_SERVER#unix:}
elif [[ -n "${XDG_RUNTIME_DIR:-}" && -S "$XDG_RUNTIME_DIR/pulse/native" ]]; then
    pulse_socket="$XDG_RUNTIME_DIR/pulse/native"
fi
if [[ -n "$pulse_socket" && -S "$pulse_socket" ]]; then
    run_args+=(-v "$pulse_socket:$pulse_socket" -e "PULSE_SERVER=unix:$pulse_socket")
    pulse_cookie="${PULSE_COOKIE:-${XDG_CONFIG_HOME:-$HOME/.config}/pulse/cookie}"
    if [[ -r "$pulse_cookie" ]]; then
        run_args+=(-v "$pulse_cookie:$pulse_cookie:ro" -e "PULSE_COOKIE=$pulse_cookie")
    fi
fi
if [[ "$THEKERNEL_DEV_IMAGE" == "thekernel-dev:local" ]]; then
    if [[ "$force_build" == 1 ]] ||
        ! docker image inspect "$THEKERNEL_DEV_IMAGE" >/dev/null 2>&1; then
        docker compose -f "$DEV_ENV_DIR/compose.yaml" build dev
    fi
fi


# A linked worktree stores a small `.git` file whose gitdir points into the
# primary checkout. A bind of only the worktree does not include that external
# directory, so Git commands inside the container would otherwise fail. Mount
# each external common directory read-only at the same absolute path. Normal
# checkouts already carry their own .git directory and need no extra mount.
mount_linked_git_common_dir() {
    local checkout=$1
    local git_common_dir

    git_common_dir=$(
        git -C "$checkout" rev-parse --path-format=absolute --git-common-dir \
            2>/dev/null || true
    )
    if [[ -n "$git_common_dir" && "$git_common_dir" != "$checkout"/* ]]; then
        run_args+=(--volume "$git_common_dir:$git_common_dir:ro,z")
    fi
}

mount_linked_git_common_dir "$REPO_ROOT"

cd "$REPO_ROOT"
docker compose "${compose_files[@]}" "${run_args[@]}" dev "$@"
