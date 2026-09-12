# AGENTS.md — agent-trace-gateway

Built and reviewed by agents. Conventions that keep that safe live here.

## Review checklist (gatekeeper, hard-earned)

Two failure classes escaped multiple review rounds during v0.2.0→v0.3.1 and
were only caught by E2E or a self-audit. When a change of these shapes lands,
check them FIRST:

1. **Randomization of a per-entity value** (trace ids, span ids, session
   salts): every consumer that must agree on ONE entity must draw ONE sample
   and share it — two independent `random_*()` calls for the same entity
   produce silent disagreement (v0.3.0: agent span and its generation child
   got different traceIds from P0-1 randomization; determinism had hidden the
   shared value, and the unit nail "5 calls → 5 distinct ids" enshrined the
   WRONG shape). Gate check: grep all call sites of the randomized value;
   assert in a test that same-entity consumers share equality AND that the
   value stays distinct across entities.

2. **A declared table field is not a consumed behavior** (descriptor rows,
   config keys, registry entries): a row in a table plus a green unit test of
   the row's reader does NOT prove the production path evaluates it
   (v0.2.x→v0.3.0: the anthropic descriptor declared `usage_frames` and the
   reader was unit-tested green, but the engine harvested usage only inside a
   `Usage` SSE action the anthropic table never had — streaming anthropic
   usage was silently dropped for the entire descriptor era). Gate check:
   trace one real wire input through the ENGINE/production call path per
   table field class (text, tools, usage, error, session), not just the
   table reader.

Both were found by E2E (otel-col wire inspection) and cross-path self-audit,
not by unit-shape review. Prefer wire-level assertions in `tests/otlp_export.rs`
for anything that crosses the export boundary.

## Engineering discipline: the request path must be panic-free

Code that runs PER REQUEST (filters, detection, engine) may never use bare
index slicing or `unwrap`/`expect` on fallible conversions — one malformed
input turns a miss into a dropped request (v0.3.7 production incident:
`segments[start..start+ep.len()]` in loose path detection panicked on short
paths and every probe request died with zero delivery). Use
`windows()`/iterators, `get()`, and `match` instead; identification
failures degrade to "unknown protocol, forward transparently"
(`detect_path_fail_open`). New per-request code needs a total-input-space
test (empty/deep/unicode/pathological shapes) before merge.

Review log with signatures: see the session review report
(`.tmp-atg-rust-review.md` in the working workspace) — §D/§E/§F contain the
full BLOCK/AMBIGUITY history of the descriptor, P0-semantics, and three-layer
batches.
