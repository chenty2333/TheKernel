ip="127.0.0.1"
port=12865

./busybox echo "#### OS COMP TEST GROUP START netperf ####"

run_netperf() {
    echo "====== netperf $1 begin ======"
    ./netperf -H $ip -p $port -t $1 -l 1 -- $2
    if [ $? == 0 ]; then
        ans="success"
    else
        ans="fail"
    fi
    echo "====== netperf $1 end: $ans ======"
}

wait_netserver() {
    tries=0
    while [ "$tries" -lt 10 ]; do
        ./netperf -H $ip -p $port -t TCP_RR -l -1 -- -r 1,1 >/dev/null 2>&1 && return 0
        tries=$((tries + 1))
        oscomp-sleep 1 2>/dev/null || ./busybox sleep 1
    done
    return 1
}

./netserver -D -L $ip -p $port &
server_pid=$!

wait_netserver || echo "netserver readiness check failed"

run_netperf UDP_STREAM  "-s 16k -S 16k -m 1k -M 1k"
run_netperf TCP_STREAM  "-s 16k -S 16k -m 1k -M 1k"
run_netperf UDP_RR      "-s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1"
run_netperf TCP_RR      "-s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1"
run_netperf TCP_CRR     "-s 16k -S 16k -m 1k -M 1k -r 64,64 -R 1"

kill -9 $server_pid

./busybox echo "#### OS COMP TEST GROUP END netperf ####"
