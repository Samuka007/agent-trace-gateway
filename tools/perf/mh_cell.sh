#!/usr/bin/env bash
# mh_cell.sh — one multi-hop A/B cell for the ATG#1 verification.
#
#              [frame_filler_B] [frames]
#   mh_cell.sh <binary> <label> <threads> <table_chains> [load_n] [conc] [msgs] [history_bytes]
#
# Topology (the multi-hop deployment ATG#1 is about):
#
#   driver -> hoprelay (L4, own TCP conn) -> ATG <binary> -> fake upstream
#
# The hop is what the single-hop bench lacked. Everything the two arms must
# share is fixed by this script: same hop, same fake, same payload generator,
# same offered concurrency, same window. The ONLY variable is <threads>.
#
# Evidence discipline (inherited from tools/perf/matrix.sh, plus two additions):
#  - the effective worker count is VERIFIED from the startup attestation line
#    for EVERY arm; mismatch aborts the cell (a silently-ignored
#    ATG_WORKER_THREADS once made a whole matrix run the wrong thread count);
#  - the stitcher's chain table is populated THROUGH the gateway before the
#    load, and its size is read back from /__atg/health — the table is the
#    independent variable of the stitcher's serial point, so a cell that
#    claims "table=50k" must prove it;
#  - /proc/<pid>/stat CPU (utime+stime) and a per-tid split via /proc/<pid>/task
#    (awk only: this container's bare shell has no perl);
#  - SIGKILL, not SIGTERM: pingora's run_forever does not exit on SIGTERM and a
#    bare `wait` then blocks the cell forever.
set -u
BIN="$1"
LABEL="$2"
THREADS="$3"
TABLE="${4:-0}"
N_LOAD="${5:-3000}"
CONC="${6:-64}"
MSGS="${7:-8}"
HIST="${8:-2048}"
FRAME_FILLER="${9:-95}"   # production delta frame is 119 B total (95 + ~24 B envelope)
FRAMES="${10:-1250}"      # production MIX weighted mean (rustfake SHAPE_A/B/C/D)
PORT=6199
HOP=6299
UP=19889
DELAY_MS=1
RUN=/tmp/atg-perf

proc_ticks() { awk '{ sub(/^.*\)/, ""); print $12 + $13 }' "/proc/$1/stat" 2>/dev/null; }

tid_cpu() {
    for t in "/proc/$1/task/"*; do
        [ -r "$t/stat" ] || continue
        awk -v f="$t" '{
            sub(/^.*\)/, "")
            n = split(f, a, "/")
            printf "%s %d\n", a[n-1], $12 + $13
        }' "$t/stat"
    done
}

json_field() { # <json> <key>
    awk -v s="$1" -v key="$2" '
        BEGIN {
            p = index(s, "\"" key "\":")
            if (p == 0) { print -1; exit }
            s = substr(s, p + length(key) + 3)
            i = index(s, ",")
            j = index(s, "}")
            if (i == 0 || (j > 0 && j < i)) i = j
            print (i > 0 ? substr(s, 1, i - 1) : s) + 0
        }'
}

pkill -f "[u]pstream -port $UP" >/dev/null 2>&1
pkill -f "[h]oprelay -listen" >/dev/null 2>&1
sleep 0.3

"$RUN/upstream" -port "$UP" -delay-ms "$DELAY_MS" -sse-bytes "$FRAME_FILLER" -sse-frames "$FRAMES" \
    >"$RUN/$LABEL.upstream.log" 2>&1 &
UP_PID=$!
"$RUN/hoprelay" -listen "127.0.0.1:$HOP" -target "127.0.0.1:$PORT" >"$RUN/$LABEL.hoprelay.log" 2>&1 &
HOP_PID=$!
sleep 0.5

ATG_WORKER_THREADS="$THREADS" ATG_LISTEN="127.0.0.1:$PORT" ATG_UPSTREAM="127.0.0.1:$UP" \
    ATG_STITCH_CAPACITY=200000 \
    "$BIN" >"$RUN/$LABEL.log" 2>&1 &
GW=$!
sleep 1.5

echo "==================== MH $LABEL (threads=$THREADS table=$TABLE c=$CONC n=$N_LOAD msgs=$MSGS histB=$HIST) ===================="
ATTEST=$(head -1 "$RUN/$LABEL.log")
echo "startup: $ATTEST"
echo "binary: $BIN sha256=$(sha256sum "$BIN" | awk '{print $1}')"
ATTEST_THREADS=$(awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^worker_threads=/) { sub(/^worker_threads=/, "", $i); print $i } }' <<<"$ATTEST")
echo "attestation_worker_threads=$ATTEST_THREADS (requested=$THREADS)"
if [ "$ATTEST_THREADS" != "$THREADS" ]; then
    echo "ABORT: attestation mismatch"
    kill -9 "$GW" "$UP_PID" "$HOP_PID" >/dev/null 2>&1
    wait "$GW" "$UP_PID" "$HOP_PID" 2>/dev/null
    exit 1
fi
TIDS=$(ps -L -o tid= -p "$GW" 2>/dev/null | wc -l)
echo "gateway_tids_before_load=$TIDS"

# Actual upstream shape: frame count and per-frame bytes MEASURED from a live
# response, so the label cannot claim a shape the rig did not run (the
# single-hop rig once shipped a cosmetic frame-size argument and mislabelled
# two whole tables).
UP_SHAPE=$(curl -s "http://127.0.0.1:$UP/" | awk '
    { buf = buf $0 "\n" }
    END {
        n = gsub(/\n\n/, "", buf)
        printf "data_frames=%d bytes=%d avg_frame_B=%.1f", n, length(buf), length(buf) / (n > 0 ? n : 1)
    }')
echo "upstream_shape: $UP_SHAPE (production delta frame = 119 B, ~1261 frames)"

if [ "$TABLE" -gt 0 ]; then
    echo "--- populate $TABLE distinct chains (through the hop) ---"
    "$RUN/drive_mh" -mode populate -url "http://127.0.0.1:$HOP/v1/messages" \
        -n "$TABLE" -c 32 -msgs "$MSGS" -history-bytes "$HIST"
fi

HEALTH0=$(curl -s "http://127.0.0.1:$PORT/__atg/health")
echo "stitch_entries_before=$(json_field "$HEALTH0" stitch_entries) (requested table=$TABLE)"
METRICS0=$(curl -s "http://127.0.0.1:$PORT/__atg/metrics")

echo "--- load: n=$N_LOAD c=$CONC distinct=100 msgs=$MSGS histB=$HIST (driver through the hop) ---"
T0=$(proc_ticks "$GW")
tid_cpu "$GW" >"$RUN/$LABEL.tid.before"
WALL0=$(date +%s.%N)
"$RUN/drive_mh" -mode load -url "http://127.0.0.1:$HOP/v1/messages" \
    -n "$N_LOAD" -c "$CONC" -distinct 100 -msgs "$MSGS" -history-bytes "$HIST"
WALL1=$(date +%s.%N)
T1=$(proc_ticks "$GW")
tid_cpu "$GW" >"$RUN/$LABEL.tid.after"
HEALTH1=$(curl -s "http://127.0.0.1:$PORT/__atg/health")
METRICS=$(curl -s "http://127.0.0.1:$PORT/__atg/metrics")

awk -v t0="$T0" -v t1="$T1" -v n="$N_LOAD" -v w0="$WALL0" -v w1="$WALL1" 'BEGIN {
    cpu_s = (t1 - t0) / 100.0
    wall = w1 - w0
    printf "process_cpu_ms_total=%.1f cpu_ms_per_req=%.4f gateway_cores_used=%.2f (wall=%.2fs)\n", \
        (t1 - t0) * 10, (t1 - t0) * 10 / n, (wall > 0 ? cpu_s / wall : 0), wall
}'

awk -v a="$HEALTH0" -v b="$HEALTH1" -v n="$N_LOAD" '
    function field(s, key,   i, p) {
        p = index(s, "\"" key "\":")
        if (p == 0) return -1
        s = substr(s, p + length(key) + 3)
        i = index(s, ","); j = index(s, "}")
        if (i == 0 || (j > 0 && j < i)) i = j
        return (i > 0 ? substr(s, 1, i - 1) : s) + 0
    }
    BEGIN {
        wt = field(b, "stitch_wait_ns_total") - field(a, "stitch_wait_ns_total")
        hd = field(b, "stitch_hold_ns_total") - field(a, "stitch_hold_ns_total")
        tt = field(b, "turns_total") - field(a, "turns_total")
        printf "stitch_entries_after=%d turns_delta=%d stitch_wait_us/req=%.2f stitch_hold_us/req=%.2f\n", \
            field(b, "stitch_entries"), tt, (tt > 0 ? wt / 1000.0 / tt : 0), (tt > 0 ? hd / 1000.0 / tt : 0)
    }'

# Hop-level first-byte time INSIDE ATG (ATG#1 Specification 2): the time from
# the request entering ATG to the first response byte leaving it
# (completion_start_ns - start_ns, aggregated by atg_stage_time_to_first_byte).
#
# DELTA, not cumulative: the counter is process-lifetime and would otherwise be
# dominated by the 100k-chain populate phase that precedes the load window.
# The buckets are cumulative counts, so subtracting bucket-wise yields a real
# window distribution; p50 is interpolated inside the bucket that crosses the
# half-way count (bucket width bounds the error — stated, not hidden).
echo "atg_internal_ttfb (window delta, ATG#1 spec 2):"
printf '%s\n' "$METRICS0" | awk '
    /^atg_stage_time_to_first_byte_seconds_bucket/ {
        match($0, /le="[^"]*"/); le = substr($0, RSTART + 4, RLENGTH - 5)
        match($0, / [0-9]+$/); b0[le] = substr($0, RSTART + 1)
    }
    /^atg_stage_time_to_first_byte_seconds_count/ { match($0, / [0-9]+$/); c0 = substr($0, RSTART + 1) }
    /^atg_stage_time_to_first_byte_seconds_sum/   { match($0, / [0-9.e+-]+$/); s0 = substr($0, RSTART + 1) }
    END { for (k in b0) print "B0", k, b0[k]; print "C0", c0; print "S0", s0 }
' >"$RUN/$LABEL.ttfb0.awk"
printf '%s\n' "$METRICS" | awk '
    /^atg_stage_time_to_first_byte_seconds_bucket/ {
        match($0, /le="[^"]*"/); le = substr($0, RSTART + 4, RLENGTH - 5)
        match($0, / [0-9]+$/); b1[le] = substr($0, RSTART + 1)
    }
    /^atg_stage_time_to_first_byte_seconds_count/ { match($0, / [0-9]+$/); c1 = substr($0, RSTART + 1) }
    /^atg_stage_time_to_first_byte_seconds_sum/   { match($0, / [0-9.e+-]+$/); s1 = substr($0, RSTART + 1) }
    END { for (k in b1) print "B1", k, b1[k]; print "C1", c1; print "S1", s1 }
' >"$RUN/$LABEL.ttfb1.awk"
awk '
    function bound(s) { gsub(/"/, "", s); return (s == "+Inf" ? 1e18 : s + 0) }
    FILENAME ~ /ttfb0/ {
        $1 == "B0" { b0[$2] = $3; next }
        $1 == "C0" { c0 = $2; next }
        $1 == "S0" { s0 = $2; next }
    }
    FILENAME ~ /ttfb1/ {
        $1 == "B1" { b1[$2] = $3; next }
        $1 == "C1" { c1 = $2; next }
        $1 == "S1" { s1 = $2; next }
    }
    END {
        n = 0; prev = 0; target = 0; p50 = -1; p90 = -1; p99 = -1
        cnt = c1 - c0
        for (k in b1) { key[++n] = k }
        # selection sort by numeric bucket bound
        for (i = 1; i < n; i++) for (j = i + 1; j <= n; j++)
            if (bound(key[j]) < bound(key[i])) { t = key[i]; key[i] = key[j]; key[j] = t }
        for (i = 1; i <= n; i++) {
            d = b1[key[i]] - (key[i] in b0 ? b0[key[i]] : 0)
            hi = bound(key[i])
            if (p50 < 0 && d * 1e3 >= cnt * 500) p50 = hi * 1e3
            if (p90 < 0 && d * 1e3 >= cnt * 900) p90 = hi * 1e3
            if (p99 < 0 && d * 1e3 >= cnt * 990) p99 = hi * 1e3
        }
        mean_ms = (cnt > 0 ? ((s1 - s0) / cnt) * 1e3 : 0)
        printf "  window_turns=%d mean=%.2fms p50<=%.2fms p90<=%.2fms p99<=%.2fms (bucket-upper-bound)\n", \
            cnt, mean_ms, p50, p90, p99
        for (i = 1; i <= n; i++)
            printf "  le=%-8s delta=%d\n", key[i], b1[key[i]] - (key[i] in b0 ? b0[key[i]] : 0)
    }
' "$RUN/$LABEL.ttfb0.awk" "$RUN/$LABEL.ttfb1.awk"
echo "atg_inflight_high_water=$(json_field "$HEALTH1" inflight_high_water) awaiting_upstream=$(json_field "$HEALTH1" awaiting_upstream)"

awk -v before="$RUN/$LABEL.tid.before" -v after="$RUN/$LABEL.tid.after" '
    BEGIN {
        while ((getline line < before) > 0) { split(line, f, " "); b[f[1]] = f[2] }
        close(before)
        tot = 0; k = 0
        while ((getline line < after) > 0) {
            split(line, f, " ")
            d = f[2] - (f[1] in b ? b[f[1]] : 0)
            if (d <= 0) continue
            k++; tid[k] = f[1]; cpu[k] = d; tot += d
        }
        close(after)
        for (i = 1; i < k; i++) for (j = i + 1; j <= k; j++) if (cpu[j] > cpu[i]) {
            s = cpu[i]; cpu[i] = cpu[j]; cpu[j] = s; t = tid[i]; tid[i] = tid[j]; tid[j] = t
        }
        printf "thread_cpu_ms_total=%.1f threads_with_cpu=%d\n", tot * 10, k
        lim = (k < 12 ? k : 12)
        for (i = 1; i <= lim; i++) printf "  tid=%s cpu_ms=%.1f (%.1f%%)\n", tid[i], cpu[i] * 10, (tot > 0 ? 100 * cpu[i] / tot : 0)
    }'

kill -9 "$GW" "$UP_PID" "$HOP_PID" >/dev/null 2>&1
wait "$GW" "$UP_PID" "$HOP_PID" 2>/dev/null
echo "==================== END MH $LABEL ===================="
