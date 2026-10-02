#!/usr/bin/env bash
# Screen evidence for a DUT whose only output is a screen.
#
# Two subcommands, because the two halves of screen evidence come from
# different places: the frames come from a driver-free USB HDMI capture dongle
# on the development host, and the verdict comes from whatever read them.
#
#   n305-screen-capture.sh capture --out DIR [--seconds N] [--device DEVICE]
#       Record the screen as frame-0000.ppm, frame-0001.ppm, ...  Run it before
#       the DUT powers on and stop it after it has finished; the reader (a
#       person now, a checker later) needs frames from before the completion
#       banner to after it.
#
#   n305-screen-capture.sh preview [--device DEVICE]
#       Open a live ffplay window for the capture dongle.  This is useful while
#       a person is waiting to press the DUT's power button.
#
#   n305-screen-capture.sh verdict --dir DIR --completion-frame N --checker NAME
#       Write the verdict.txt the DUT gate requires, from the frames actually
#       present in DIR.  --completion-frame is the index of the first frame in
#       which the completion banner is legible; the newest frame becomes
#       last_frame, which is the evidence that the capture kept looking after
#       the banner appeared.
#
# The gate validates the result independently: real P6 PPM files, a declared
# resolution that matches, frames that are not blank, a contiguous frame
# sequence, and a last frame that follows the completion frame.

set -euo pipefail

# What the checker looked for on the screen.  The transcript marker is what
# the system profile prints last; the shell profile's readiness prompt is what
# a screen-only acceptance run uses instead.
MARKER="# THEKERNEL_SYSTEM_TEST_COMPLETE"

default_device() {
	# The HDMI dongle exposes two V4L nodes: index0 is the MJPEG video stream
	# and index1 is metadata/control.  Prefer the stable by-id node so camera
	# enumeration cannot silently select /dev/video0.
	for candidate in \
		/dev/v4l/by-id/*eEver*video-index0 \
		/dev/v4l/by-id/*HDMI*video-index0 \
		/dev/video4 \
		/dev/video0; do
		[ -e "$candidate" ] || continue
		printf '%s' "$candidate"
		return 0
	done
	return 1
}

capture_device_args() {
	# The known eEver dongle advertises only MJPEG at 1920x1080.  Other V4L
	# devices retain ffmpeg's normal format negotiation.
	case "$1" in
		*eEver*|/dev/video4)
			printf '%s\n' '-input_format' 'mjpeg' '-video_size' '1920x1080' '-framerate' '30'
			;;
	esac
}

die() {
	printf 'n305-screen-capture: %s\n' "$*" >&2
	exit 1
}

usage() {
	sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
	exit "${1:-0}"
}

command=${1:-}
[ -n "$command" ] || usage 2
shift

case "$command" in
capture)
	device=$(default_device) || device=/dev/video0
	out=""
	seconds=0
	while [ $# -gt 0 ]; do
		case "$1" in
		--device)
			device="$2"
			shift 2
			;;
		--out)
			out="$2"
			shift 2
			;;
		--seconds)
			seconds="$2"
			shift 2
			;;
		*) die "unknown argument: $1" ;;
		esac
	done
	[ -n "$out" ] || die "--out is required"
	command -v ffmpeg >/dev/null 2>&1 || die "ffmpeg is required to read the capture dongle"
	[ -e "$device" ] || die "no capture device at $device (is the HDMI dongle plugged in?)"
	mkdir -p "$out"
	# One frame per second is enough to cover a boot and cheap to check, and
	# -start_number 0 is what makes the frame indices start where the gate
	# expects them to.
	input_args=(-f v4l2)
	while IFS= read -r arg; do
		input_args+=("$arg")
	done < <(capture_device_args "$device")
	set -- "${input_args[@]}" -i "$device" -vf fps=1 -start_number 0 "$out/frame-%04d.ppm"
	if [ "$seconds" != 0 ]; then
		set -- "$@" -t "$seconds"
	fi
	printf 'n305-screen-capture: recording %s to %s (interrupt when the DUT has finished)\n' \
		"$device" "$out"
	exec ffmpeg -hide_banner -loglevel warning -y "$@"
	;;
preview)
	device=$(default_device) || device=/dev/video0
	while [ $# -gt 0 ]; do
		case "$1" in
		--device)
			device="$2"
			shift 2
			;;
		*) die "unknown argument: $1" ;;
		esac
	done
	command -v ffplay >/dev/null 2>&1 || die "ffplay is required for the live preview"
	[ -e "$device" ] || die "no capture device at $device (is the HDMI dongle plugged in?)"
	input_args=(-f v4l2)
	while IFS= read -r arg; do
		input_args+=("$arg")
	done < <(capture_device_args "$device")
	exec ffplay -hide_banner -loglevel warning "${input_args[@]}" \
		-window_title 'N305 HDMI capture' "$device"
	;;
verdict)
	dir=""
	completion=""
	checker=""
	dut=n305
	run=1
	resolution=""
	marker="$MARKER"
	while [ $# -gt 0 ]; do
		case "$1" in
		--dir)
			dir="$2"
			shift 2
			;;
		--completion-frame)
			completion="$2"
			shift 2
			;;
		--checker)
			checker="$2"
			shift 2
			;;
		--dut)
			dut="$2"
			shift 2
			;;
		--run)
			run="$2"
			shift 2
			;;
		--resolution)
			resolution="$2"
			shift 2
			;;
		--marker)
			marker="$2"
			shift 2
			;;
		*) die "unknown argument: $1" ;;
		esac
	done
	[ -n "$dir" ] && [ -n "$completion" ] && [ -n "$checker" ] ||
		die "--dir, --completion-frame and --checker are all required"
	[ -d "$dir" ] || die "no such frame directory: $dir"

	frames=()
	while IFS= read -r frame; do
		frames+=("$frame")
	done < <(find "$dir" -maxdepth 1 -name 'frame-*.ppm' | sort)
	[ "${#frames[@]}" -ge 2 ] || die "need at least two frames in $dir, found ${#frames[@]}"

	index=$(printf '%04d' "$completion")
	completion_name="frame-$index.ppm"
	[ -f "$dir/$completion_name" ] || die "no such frame: $dir/$completion_name"
	last_name=$(basename "${frames[${#frames[@]} - 1]}")
	[ "$last_name" != "$completion_name" ] ||
		die "the completion frame is also the last frame; the capture must keep looking afterwards"

	if [ -z "$resolution" ]; then
		# P6 header: magic, width, height, maxval.
		resolution=$(head -c 64 "$dir/$last_name" | tr '\n' ' ' |
			awk '{ if ($1 != "P6") exit 1; print $2 "x" $3 }') ||
			die "cannot read the PPM header of $last_name"
	fi

	cat > "$dir/verdict.txt" <<EOF
# Written by scripts/ci/n305-screen-capture.sh.  The checker named below is
# what asserted that $completion_name shows $marker.
version=1
dut=$dut
run=$run
resolution=$resolution
frames=${#frames[@]}
completion_frame=$completion_name
last_frame=$last_name
marker=$marker
checker=$checker
EOF
	printf 'n305-screen-capture: wrote %s/verdict.txt (completion %s, last %s, %s frames)\n' \
		"$dir" "$completion_name" "$last_name" "${#frames[@]}"
	;;
-h | --help | "")
	usage 0
	;;
*)
	die "unknown subcommand: $command"
	;;
esac
