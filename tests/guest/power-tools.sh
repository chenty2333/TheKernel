#!/bin/sh
# Actual unmodified Alpine binaries. Unsupported hardware is an expected
# diagnostic, not evidence that frequency/temperature measurements work.
set -eu
export LC_ALL=C
out=/tmp/thekernel-power-tool.out
cpupower frequency-info >"$out" 2>&1
cat "$out"
grep -q 'analyzing CPU' "$out"
if [ "$(cat /sys/devices/system/cpu/cpu0/cpufreq_supported)" = 0 ]; then
    grep -q 'no or unknown cpufreq driver is active' "$out"
else
    grep -q 'intel_pstate' "$out"
fi
cpupower idle-info >"$out" 2>&1
cat "$out"
grep -q 'CPUidle driver:' "$out"
grep -q 'Available idle states:' "$out"
grep -q 'HLT' "$out"
if [ "$(cat /sys/devices/system/cpu/cpuidle/mwait_enabled)" = 1 ] &&
   [ "$(cat /sys/devices/system/cpu/cpu0/cpuidle/mwait_supported)" = 1 ]; then
    grep -q 'MWAIT' "$out"
fi
result=0
sensors >"$out" 2>&1 || result=$?
cat "$out"
if [ "$result" = 1 ]; then
    grep -q 'No sensors found' "$out"
    [ ! -e /sys/class/hwmon/hwmon0/temp1_input ]
else
    [ "$result" = 0 ]
    grep -q 'coretemp' "$out"
    grep -q 'Core' "$out"
fi
rm -f "$out"
echo THEKERNEL_REAL_CPU_TOOLS_OK
