#!/usr/bin/env bash
# Screen evidence for a DUT whose only output is a screen.
#
# Two subcommands, because the two halves of screen evidence come from
# different places: the frames come from a driver-free USB HDMI capture dongle
# on the development host, and the verdict comes from whatever read them.
#
#   n305-screen-capture.sh capture --out DIR [--seconds N] [--device /dev/video0]
#       Record the screen as frame-0000.ppm, frame-0001.ppm, ...  Run it before
#       the DUT powers on and stop it after it has finished; the reader (a
#       person now, a checker later) needs frames from before the completion
#       banner to after it.
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

MARKER="# THEKERNEL_SYSTEM_TEST_COMPLETE"

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
	device=/dev/video0
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
	set -- -f v4l2 -i "$device" -vf fps=1 -start_number 0 "$out/frame-%04d.ppm"
	if [ "$seconds" != 0 ]; then
		set -- "$@" -t "$seconds"
	fi
	printf 'n305-screen-capture: recording %s to %s (interrupt when the DUT has finished)\n' \
		"$device" "$out"
	exec ffmpeg -hide_banner -loglevel warning -y "$@"
	;;
verdict)
	dir=""
	completion=""
	checker=""
	dut=n305
	run=1
	resolution=""
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
# what asserted that $completion_name shows $MARKER.
version=1
dut=$dut
run=$run
resolution=$resolution
frames=${#frames[@]}
completion_frame=$completion_name
last_frame=$last_name
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
