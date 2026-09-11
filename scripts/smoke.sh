#!/usr/bin/env bash
set -u
export LIBCLANG_PATH=/nix/store/63vszz1y8lw6xqjipsw6bh56105nw894-clang-21.1.8-lib/lib
cd /home/nixos/workspace/scitrace/agent-trace-gateway/.worktrees/atcd
pkill -f mock_upstream 2>/dev/null
pkill -f "debug/atcd serve" 2>/dev/null
sleep 0.5
BIN=$PWD/target/debug/atcd
FUT=$(( $(date +%s) + 86400 ))
rm -f /tmp/atcd-t4.db* /tmp/mock_seen.json /tmp/mock_body.txt /tmp/serve_err.log

echo "{\"account_id\":\"acc-x\",\"refresh_token\":\"rt\",\"access_token\":\"at-x\",\"expires_at\":$FUT}" \
  | ATCD_DB=/tmp/atcd-t4.db "$BIN" import

python3 scripts/mock_upstream.py > /tmp/mock_err.log 2>&1 & MOCK=$!
ATCD_DB=/tmp/atcd-t4.db ATCD_UPSTREAM=http://127.0.0.1:8499 ATCD_LISTEN=127.0.0.1:8404 \
  ATCD_MIN_TURN_GAP_MS=50 ATCD_JITTER_MS=0 "$BIN" serve 2>/tmp/serve_err.log & SERVE=$!
sleep 1.5
echo "── 路径A：codex 下游（透传）──"
curl -sS --max-time 5 -X POST http://127.0.0.1:8404/v1/responses \
  -H 'session-id: 01991aaa-downstream-session' \
  -H 'x-codex-installation-id: their-install-uuid' \
  -H 'x-codex-turn-metadata: {"installation_id":"their-install-uuid","session_id":"01991aaa-downstream-session","turn_id":"t1","sandbox":"workspace-write"}' \
  -H 'authorization: Bearer sk-d' -d '{"model":"m","input":[]}' ; echo
echo "── 路径B：第三方 agent（铸造）──"
curl -sS --max-time 5 -X POST http://127.0.0.1:8404/v1/responses \
  -H 'authorization: Bearer sk-d' \
  -H 'user-agent: opencode/1.0' \
  -d '{"model":"m","input":[],"prompt_cache_key":"omp-conv-1"}' ; echo
sleep 0.3
echo "── 路径A 头 ──"; cat /tmp/mock_seen.json 2>/dev/null
kill $MOCK $SERVE 2>/dev/null
wait 2>/dev/null
echo "── serve stderr ──"; cat /tmp/serve_err.log
echo "── accounts ──"
ATCD_DB=/tmp/atcd-t4.db "$BIN" accounts
