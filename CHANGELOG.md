# Changelog

本项目的所有显著变更记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 计划中（台账，未排期）

- **bench 进 CI**：sse_bench 目前 debug 自 ignore、release 手跑（数字 372–787µs，已由 RustGate 本机独立复核真实）；挂进 CI release job 或 xtask 以防门禁失效
- **openai.live descriptor 死数据**：sse_rules/usage_frames 无消费者（ws.rs 仍手写 match，正确但属第二协议分支点）——二选一：WsTurnState 接引擎（加 TurnMarkers/EndTurn action）或删表字段注明不可表
- **error 标记扩展**：response.failed/incomplete 仅落 TurnRecord.error，generation span status 与 error.type 属性扩展待做
- **SseAction::Usage 死载荷**：引擎忽略路径参数改 unit 变体（API 卫生）
- **req_buf 多次 parse 收敛**：logging 钩子对 req_buf 的 session/user_input/messages 三次 parse 合并为一次（C14 全量纪律的 req 侧）

## [0.2.1] - 2026-09-08

内部重构 + 修复 + 性能门禁（无 API 语义变化）。RustGate 审阅 4 轮（初审 9 BLOCK → 复审2 清 4 → 复审3 残 1 → 复审4 PASS）。

### 变更

- **ProtocolDescriptor 架构**：协议知识收进编译期数据表（`src/trace/descriptor.rs`，anthropic/responses/chat/openai.live 四表）+ 唯一解释引擎（`src/trace/engine.rs`）；unpack/session/adaptor 的协议 match 全删（unpack −302 行）。会话来源按语义分层（Root > StableAffinity `pck:` > ResponseChain(跳过) > EndUser）— `998d72f`、`1c8e3f1`

### 修复

- **openai cache tokens**（slop#3）：`*_details.cached_tokens` 嵌套路径表驱动解析（responses/chat/live 三钉子测试）
- **CRLF SSE 分帧**（BLOCK-A）：行驱动分帧使 LF-LF 与 CRLF-CRLF 分隔符等价——CRLF body 经规范化代理不再塌缩单帧丢流；RustGate 探针收编 `tests/crlf_framing.rs` — `bcf6bc8`
- **frame_errors 可观测**（BLOCK-B）：reassemble_sse 透传 frame_errors → Gateway.failed_frames 计数 + `/__atg/health` 暴露 + 采样日志（首条+每 10 条）— `bcf6bc8`、`3595791`
- **错误标记**（slop#6）：response.failed/incomplete + anthropic event:error 落 `TurnRecord.error` — `da69ee9`
- **responses final_output**：跨 item 拼接全部 output_text（修复多块丢正文）— `1c8e3f1`
- **zero-copy SSE 帧**：UTF-8 合法路径借用 body（修复 a3ec31e 空提交欠账）— `bcf6bc8`

### 性能

- descriptor 引擎 micro-bench（`tests/sse_bench.rs`，release 3 轮中位）：SSE 264KB 文本重组 **372µs**（v0.2.0 基线 1.7ms）；anthropic 3000 帧/50 工具流 2.78ms — `53864d8`

## [0.2.0] - 2026-09-08

首个带 tag 的发布基线。fork main 自 v0.1.0（`df0d32a`）以来的 5 项语义变更，全部经本地 dirty E2E 双线对齐验收（ATG vs sub2api modeltrace，ALL PASS）。

### 新增

- **Langfuse adaptor**：usage 提取与 generation 子 span（agent.turn + generation 两级 span、usage_details、无 gen_ai.* 前缀）— `3eef71c`

### 修复

- **OTLP 词表对齐 sub2api modeltrace**（G1–G4，Langfuse 双跑轨迹可比）— `eb0ecde`
- **OTLP endpoint 处理**：裸 host 自动补 `/v1/traces`；导出失败记录日志（不再静默）— `1f06b99`
- **Anthropic tool_use**：content_block 提取为结构化 `tool_calls` 属性（`8f64b1a`）；`content_block_delta` wire format 对齐，input_json_delta 真正命中（`72c7ba7`）
- **OpenAI Responses 裸 input items**：无 `type` 字段的 item 按 role 提取 user_input — `9a53517`

### 已知开放项

- openai_responses 协议 turn 记录 gap（usage few-shot 2/3 PASS），修复已转 GapFix lane。

## [0.1.0] - 2026-09-08

初始版本（`df0d32a`）：Pingora 反代 + 三协议 SSE 重组 + prefix 拼接 + OTLP 导出。
