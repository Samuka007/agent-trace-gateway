#!/usr/bin/env bash
# 实验：捕获真实 codex CLI 在 custom provider (wire_api=responses) 模式下
# 实际发出的请求头与 body。上游响应是假的 200，codex 收到后会报错——
# 我们只要请求形状。
set -u
cd /home/nixos/workspace/scitrace/agent-trace-gateway/.worktrees/atcd
pkill -f mock_upstream 2>/dev/null; sleep 0.5

HOME_DIR=/tmp/codex-capture-home
rm -rf "$HOME_DIR" /tmp/mock_seen.json /tmp/mock_body.txt
mkdir -p "$HOME_DIR"
cat > "$HOME_DIR/config.toml" <<TOML
model = "gpt-5.5"
model_provider = "capture"

[model_providers.capture]
name = "capture"
base_url = "http://127.0.0.1:8499/v1"
wire_api = "responses"
env_key = "CAPTURE_KEY"
TOML

python3 scripts/mock_upstream.py > /tmp/mock_err.log 2>&1 & MOCK=$!
sleep 0.8

mkdir -p /tmp/codex-capture-cwd
cd /tmp/codex-capture-cwd
CODEX_HOME="$HOME_DIR" CAPTURE_KEY=dummy-key-for-shape-capture \
  timeout 60 codex exec --skip-git-repo-check "say ok" > /tmp/codex_out.log 2>&1
echo "── codex 退出码: $? ──"
tail -5 /tmp/codex_out.log
echo "── codex 实际发出的请求头 ──"
cat /tmp/mock_seen.json 2>/dev/null || echo "(没有请求到达 mock)"
echo "── codex 实际发出的 body（前 1500 字节）──"
head -c 1500 /tmp/mock_body.txt 2>/dev/null || echo "(无 body)"
echo
kill $MOCK 2>/dev/null
