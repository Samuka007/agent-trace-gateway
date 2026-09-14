#!/usr/bin/env bash
# Measurement-only ablations for the #4 observability surface (ATG#5 /
# ATG#2 ±5% gate): neutralize the per-request observation work one layer at a
# time, then rebuild. Applied inside the container tree only — never committed.
#   abl.sh observe   -> Histogram::observe becomes a no-op (histograms: 12 RMW/req)
#   abl.sh gauges    -> enter_request/exit_request become no-ops (5 RMW/req)
set -eu
cd /opt/atcd
case "${1:-}" in
observe)
    perl -0pi -e 's/pub fn observe\(&self, ns: u64\) \{/pub fn observe(\&self, _ns: u64) {\n        if true { return; }/' src/metrics.rs
    grep -q 'if true { return; }' src/metrics.rs || { echo "observe-ablation: FAILED"; exit 1; }
    echo "observe-ablation: applied"
    ;;
gauges)
    perl -0pi -e 's/fn enter_request\(&self\) \{/fn enter_request(\&self) {\n        if true { return; }/' src/lib.rs
    perl -0pi -e 's/fn exit_request\(&self, ctx: &Ctx, identified: bool\) \{/fn exit_request(\&self, _ctx: \&Ctx, _identified: bool) {\n        if true { return; }/' src/lib.rs
    grep -q 'if true { return; }' src/lib.rs || { echo "gauges-ablation: FAILED"; exit 1; }
    echo "gauges-ablation: applied"
    ;;
*)
    echo "usage: abl.sh observe|gauges"; exit 2 ;;
esac
