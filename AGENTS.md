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

The gate is mechanical since v0.3.8: CI runs
`cargo clippy --workspace --lib --bins -- -D clippy::indexing_slicing -D
clippy::arithmetic_side_effects -D clippy::unwrap_used -D
clippy::expect_used` (non-test targets via `--lib --bins`). Legacy audited
sites carry file-level `// PANIC-AUDIT v0.3.8: …` allows with the audit
rationale and the PanicAudit issue reference — do not extend those allows
to new code, and do not strip the rationale comments.

Review log with signatures: see the session review report
(`.tmp-atg-rust-review.md` in the working workspace) — §D/§E/§F contain the
full BLOCK/AMBIGUITY history of the descriptor, P0-semantics, and three-layer
batches.

## Observability contract: header evidence (issue #32)

The trace carries **every request header except the blacklist**
(`HDR_EVIDENCE_DENY` + `HDR_EVIDENCE_DENY_MARKERS`): credential carriers,
cookies, and hop-by-hop framing. Each value is capped at **256 chars on a
char boundary** (shared helper with `client_ua`); per turn the map is bounded
by **32 entries**, a **4096-byte** name+value budget, and a **64-char** header
name cap. Bound overflow is counted in `hdr_evidence_dropped` (health) /
`atg_hdr_evidence_dropped_total` (metrics) — never silently. Blacklist hits
are policy, not overflow: they are not counted.

Rules a reviewer must enforce:

- **The blacklist is the security boundary.** Any header that can carry a
  credential belongs in `HDR_EVIDENCE_DENY` (explicit) or in the marker list
  (substring defence for client-invented names). Removing an entry is a
  security change, not a code tweak. Known false positive: rate-limit headers
  whose names mention tokens.
- **Credential plaintext never reaches a surface.** `client_key_fp` is the
  only trace-side representative; wire tests assert sentinel plaintext is
  absent from both `/__atg/records` and the OTLP payload.
- **Bounds are part of the contract** (32 entries / 4096 bytes / 64-char
  names / 256-char values): a change to any of them is a spec change — the
  metadata map must stay bounded for client-controlled input.
- **`session_source` names the winner** (`body:<path>`, `body:harness`,
  `header:<name>`, `harness-header:<name>`, `stitcher`); an empty source with
  a non-empty `session.id` is a bug.
