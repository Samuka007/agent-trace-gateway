#!/usr/bin/env bash
# ATG multi-threading matrix — one cell. Inside `nix develop -c bash`.
#   matrix2.sh <binary> <label> <threads> <table_chains> [load_n] [conc] [delay_ms]
# Reports TTFB percentiles, achieved rps, and the gateway's own CPU per request
# (utime+stime from /proc/<pid>/stat, so the per-request amplification factor
# between thread counts is measurable from the same instrument).
set -u
BIN="$1"
LABEL="$2"
THREADS="$3"
TABLE="${4:-0}"
N_LOAD="${5:-20000}"
CONC="${6:-64}"
DELAY_MS="${7:-5}"
PORT=6199
UP=19889

cpu_ticks() {
    # /proc/<pid>/stat, fields after the last ')': index 11 = utime, 12 = stime.
    perl -ne 's/^.*\)//; my @f = split; print $f[11] + $f[12]' "/proc/$1/stat" 2>/dev/null
}

pkill -f "upstream -port $UP" >/dev/null 2>&1
sleep 0.3
/tmp/atg-perf/upstream -port "$UP" -delay-ms "$DELAY_MS" >/tmp/atg-perf/upstream.log 2>&1 &
UP_PID=$!
sleep 0.5

ATG_WORKER_THREADS="$THREADS" ATG_LISTEN="127.0.0.1:$PORT" ATG_UPSTREAM="127.0.0.1:$UP" \
    "$BIN" >"/tmp/atg-perf/$LABEL.log" 2>&1 &
GW=$!
sleep 1.5
echo "==================== CELL $LABEL (threads=$THREADS table=$TABLE c=$CONC delay=${DELAY_MS}ms) ===================="
echo "startup: $(head -1 "/tmp/atg-perf/$LABEL.log")"

if [ "$TABLE" -gt 0 ]; then
    echo "--- populate $TABLE distinct chains ---"
    /tmp/atg-perf/drive -mode populate -url "http://127.0.0.1:$PORT/v1/messages" \
        -n "$TABLE" -c 32 -distinct "$TABLE"
fi

echo "--- load: n=$N_LOAD c=$CONC distinct=100 ---"
T0=$(cpu_ticks "$GW")
/tmp/atg-perf/drive -mode load -url "http://127.0.0.1:$PORT/v1/messages" \
    -n "$N_LOAD" -c "$CONC" -distinct 100
T1=$(cpu_ticks "$GW")
perl -e 'my ($t0,$t1,$n) = @ARGV; my $hz = 100; printf "gateway_cpu_ms_total=%.1f cpu_ms_per_req=%.4f (ticks %d->%d, n=%d)\n", ($t1-$t0)/$hz*1000, ($t1-$t0)/$hz*1000/$n, $t0, $t1, $n;' "$T0" "$T1" "$N_LOAD"

kill "$GW" "$UP_PID" >/dev/null 2>&1
wait >/dev/null 2>&1
echo "==================== END CELL $LABEL ===================="
