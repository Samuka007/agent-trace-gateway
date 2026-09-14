#!/usr/bin/env bash
# Measurement-only patch: make the worker-thread count settable via
# ATG_WORKER_THREADS so 1-thread vs 8-thread cells can be compared on the same
# binary. Applied inside the container tree for measurement runs only — never
# committed (PR #4 keeps its self-attested constant; the matrix instrument on
# the workstation owns the real env knob).
set -eu
cd /opt/atcd
if grep -q 'const WORKER_THREADS: usize = 8;' src/lib.rs; then
    perl -0pi -e 's/const WORKER_THREADS: usize = 8;/let worker_threads_cfg: usize = std::env::var("ATG_WORKER_THREADS")\n            .ok()\n            .and_then(|v| v.trim().parse().ok())\n            .filter(|n| *n > 0)\n            .unwrap_or(8);/' src/lib.rs
fi
if grep -q 'const WORKER_THREADS: usize = 8;' src/lib.rs; then
    echo "threads-patch: FAILED (const still present)"
    exit 1
fi
perl -0pi -e 's/WORKER_THREADS\b/worker_threads_cfg/g' src/lib.rs
grep -n 'worker_threads_cfg' src/lib.rs
echo "threads-patch: applied"
