# tools/perf — ATG load-measurement rig (ATG#5)

Reproducible rig for the question "what does multi-threading actually buy this
gateway, and what eats it". Three pieces, all std-lib only (Go for the driver
and the upstream, bash for the orchestration):

| file | role |
|---|---|
| `drive.go` | concurrent client: `-mode populate` (build up stitcher chains through the gateway) / `-mode load` (fixed conversation set); prints rps + TTFB p50/p90/p99 |
| `upstream.go` | **concurrent** fake LLM upstream with a per-request `-delay-ms` |
| `matrix.sh` | one measurement cell: start upstream + gateway, optionally populate N chains, run the load, and report the gateway's own CPU per request (utime+stime from `/proc/<pid>/stat`) |
| `abl.sh` | measurement-only source ablation (neutralize the histogram observations / the request gauges) — never committed into a build that ships |

## Build & run

```bash
# inside the repo's dev environment (needs go + rust)
go build -o /tmp/atg-perf/drive    tools/perf/drive.go
go build -o /tmp/atg-perf/upstream tools/perf/upstream.go

# one cell: <binary> <label> <threads> <table_chains> [n] [concurrency] [delay_ms]
bash tools/perf/matrix.sh ./target/release/gateway t8-table0 8 0 20000 256 1
bash tools/perf/matrix.sh ./target/release/gateway t8-table20k 8 20000 20000 256 1
```

`matrix.sh` needs the binary to honour `ATG_WORKER_THREADS`; release builds from
`main` hard-code 8. For thread-count experiments apply a measurement-only patch
(make the constant an env read) to the build tree — never to a shipped branch.

## Rules learned the hard way

1. **The fake upstream must be concurrent.** A serial upstream (accept → sleep →
   respond) caps every arm at ~1/delay: the first version of this rig used a
   serial Perl upstream and reported 189 rps for *every* cell — that was the
   upstream's ceiling, not the system under test. Use `upstream.go`.
2. **Table size is the independent variable for the stitcher question.** The
   TTL sweep cost scales with the chain table, so a fresh process cannot
   falsify it: compare *table ≈ 0* against *table = 50k* with everything else
   fixed (`drive.go -mode populate` builds the table through the gateway).
3. **Fix the ramp and record it.** A stepped offered load and a gradual ramp are
   not comparable (production ramp measurements: gradual ramp = 0 errors, a
   jump = 56% failures at the same offered rate). Every cell must state load
   shape, concurrency, delay, thread count, binary provenance (digest/commit)
   and trace mode.
4. **Verify the binary is actually rebuilt.** rsync preserves mtimes; cargo can
   then consider a changed source "fresh" and silently reuse the previous
   binary — the first "after" run of this rig measured the pre-fix binary
   (identical numbers gave it away). `touch` the sources (or check the binary
   mtime against the newest source) before measuring after a sync.
5. **Label provenance.** Local release builds of a worktree are not the
   `atg-matrix` image or a ghcr release: say which commit/tree, thread count,
   trace mode, and load shape each cell used — cross-shape division is invalid.

## What the rig has measured so far (2026-09-14, ATG#5)

Mechanism level (`tests/stitch_sweep_cost.rs`, release, per `assign()` call):

| chains in table | before fix | after fix |
|---|---|---|
| 0 | 9.2 µs | 9.9 µs |
| 5 000 | 81.8 µs | 11.6 µs |
| 20 000 | 240.9 µs | 4.7 µs |
| 100 000 | 1048.0 µs | 5.0 µs |

System level (8 workers, c=64, 5 ms upstream delay, n=20 000, same binary
except the fix): table 0 → 9050 rps / TTFB p50 6.55 ms; table 50k → **784 rps /
79.9 ms before** the fix vs **9543 rps / 6.42 ms after** it.
