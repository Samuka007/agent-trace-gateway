# Spec: R4 — WebSocket V2 透传桥

Status: done（待 PM 验收）
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

- [x] wire 设计写入 docs/atcd-design.md（D7），含 v2 帧格式与握手要点
  （§12：三种 adapter 关系、URL 归一、帧格式与出/入站类型全集、握手头、
  心跳/超时/关闭语义，全部标注 codex 源码行号）
- [x] atcd-dev 容器内 `cargo build` 退出码 0，`cargo test` 全绿（2026-09-11，
  root@10.0.100.244:/opt/atcd：build EXIT=0；test EXIT=0，50 passed / 0 failed，
  其中新增 WS 单测 4 个：accept-key RFC 6455 向量、URL 归一对拍 codex、
  上游 URL+query 透传、SOCKS URL 解析——编解码要点即握手与帧路由层）
- [x] 冒烟：mock 端到端本地 ws 回环（tests/ws_v2_bridge.rs，进程内 mock
  上游 + ProxyApp 全栈 + 真实 tokio-tungstenite 客户端）证明 v2 帧透传
  字节级保真：session.update（含 unicode/转义）、未知 type 帧、Binary 帧
  双向逐字节相等；Close(4000,"mock-done") code/reason 透传；身份头 persona
  替换与工件透传逐项断言；第二条用例证明流量经 SOCKS5 出口往返
- [x] 设计与实现偏差（如有）记录回 docs/atcd-requirements.md R4 行

## Comments（实现证据）

- 实现：src/atcd/ws.rs（桥 + URL 归一移植 + SOCKS5 隧道 + 单测）；
  proxy.rs 增 route 分发与 bind_session/extract_session_key/
  extract_downstream_installation 抽取（HTTP 路径行为不变，同一函数复用）；
  rewrite.rs strip_inbound 追加 sec-websocket-* 剥离。
- 依赖：tokio-tungstenite 0.27→0.28 + rustls-tls-webpki-roots。
  0.27 时期 openai-oss-forks tokio 层 patch 因 fork 版本号=0.28 从未生效
  （Cargo.lock 锁的是 registry 源）；0.28 起锁文件两层均为 fork git 源
  （tokio-tungstenite@0e5b2d7 + tungstenite@4fffad3，与 codex 同 rev）。
- 台账 R4 已知缺口（上游 socks 绑定）已关闭：persona.proxy_url → SOCKS5
  CONNECT 隧道（socks5/socks5h + RFC 1929 userpass），TLS 于隧道上完成；
  e2e 用例 ws_bridge_dials_upstream_through_socks5_exit 证明。
- 声明偏差（逐条见 docs/atcd-design.md §12.2-D7.5/D7.6、台账 R4 行）：
  ① v2 帧集合无 installation/client_metadata 投影点（protocol.rs:50-85、
  protocol_v2.rs:27-77 全集核对），帧层零改写；② 上行握手头复用 HTTP 的
  rewrite::apply 全集（含今日 codex v2 不发的 chatgpt-account-id /
  x-codex-installation-id——账号平面一致性优先）；③ 连接建立记账一回合，
  帧级不加节奏间隔（音频流语义）。
