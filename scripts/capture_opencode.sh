#!/usr/bin/env bash
# 实验：捕获 opencode 在 responses provider 配置下实际发出的请求。
set -u
cd /home/nixos/workspace/scitrace/agent-trace-gateway/.worktrees/atcd
pkill -f mock_upstream 2>/dev/null; sleep 0.5
BIN=$PWD/target/debug/atcd
rm -rf /tmp/oc-capture-config /tmp/mock_seen.json /tmp/mock_body.txt
mkdir -p /tmp/oc-capture-config/opencode /tmp/oc-capture-cwd
cat > /tmp/oc-capture-config/opencode/opencode.json <<'JSON'
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "capture": {
      "npm": "@ai-sdk/openai",
      "name": "Capture",
      "options": {
        "baseURL": "http://127.0.0.1:8499/v1",
        "apiKey": "dummy-key-for-shape-capture"
      },
      "models": {
        "gpt-5.5": { "name": "gpt-5.5" }
      }
    }
  },
  "model": "capture/gpt-5.5"
}
JSON

python3 scripts/mock_upstream.py > /tmp/mock_err.log 2>&1 & MOCK=$!
sleep 0.8

cd /tmp/oc-capture-cwd
export XDG_CONFIG_HOME=/tmp/oc-capture-config
export XDG_DATA_HOME=/tmp/oc-capture-data
export XDG_CACHE_HOME=/tmp/oc-capture-cache
export XDG_STATE_HOME=/tmp/oc-capture-state
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_STATE_HOME"
timeout 120 nix run nixpkgs#opencode -- run "say ok" > /tmp/oc_out.log 2>&1
echo "── opencode 退出码: $? ──"
tail -5 /tmp/oc_out.log
echo "── opencode 实际发出的请求头 ──"
cat /tmp/mock_seen.json 2>/dev/null || echo "(没有请求到达 mock)"
echo "── opencode 实际发出的 body（前 1200 字节）──"
head -c 1200 /tmp/mock_body.txt 2>/dev/null || echo "(无 body)"
echo
kill $MOCK 2>/dev/null
