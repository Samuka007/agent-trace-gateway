# Spec: R4 — WebSocket V2 透传桥

Status: in-progress
Owner: PM 验收；WS-V2 agent 实现

## Requirement

atcd 支持 codex 的 WebSocket V2 传输（realtime_websocket endpoint）作为 Responses
HTTP 之外的透传通道。忠实度标准与 B1/B4 一致：wire 透传优先，身份工件不改写，
installation 三投影点规则同样适用于 WS 通道的元数据帧。

## Specification（含依据）

- 协议真源：codex fork checkout
  `/home/nixos/.cargo/git/checkouts/codex-db571c5dd4d8f153/9f95fe1/codex-rs/codex-api/src/endpoint/realtime_websocket/`
  （protocol_v1/v2.rs、methods_v1/v2.rs、frameless_bidi 变体、mod.rs）。
  实现前先读 mod.rs 弄清 v1/v2/frameless_bidi 的关系与协商方式，出 wire 设计
  （帧格式、握手头、心跳/超时、错误语义）再动手，设计记录进 docs/atcd-design.md（D7）。
- 依赖已就绪：Cargo.toml 已 patch 双层 tungstenite fork（openai-oss-forks，与
  codex 同 rev），tokio-tungstenite 0.27。
- 已知缺口（台账 R4 备注）：上游 socks 绑定——上游 WS 连接需复用 persona.proxy_url
  的 socks 出口（与 HTTP 路径 client_for 行为一致）。
- 边界：不改 HTTP 路径既有行为；WS 是新增监听/升级路径。

## Acceptance

- [ ] wire 设计写入 docs/atcd-design.md（D7），含 v2 帧格式与握手要点
- [ ] atcd-dev 容器内 `cargo build` 退出码 0，`cargo test` 全绿（新增 WS 单测含协议帧编解码用例）
- [ ] 冒烟：mock 端到端（可用 scripts/mock_upstream.py 思路或本地 ws 回环）证明 v2 帧透传字节级保真
- [ ] 设计与实现偏差（如有）记录回 docs/atcd-requirements.md R4 行
