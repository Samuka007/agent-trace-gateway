# Changelog

本项目的所有显著变更记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 计划中（台账，未排期）

- **bench 进 CI**：sse_bench 目前 debug 自 ignore、release 手跑（数字 372–787µs，已由 RustGate 本机独立复核真实）；挂进 CI release job 或 xtask 以防门禁失效
- **openai.live descriptor 死数据**：sse_rules/usage_frames 无消费者（ws.rs 仍手写 match，正确但属第二协议分支点）——二选一：WsTurnState 接引擎（加 TurnMarkers/EndTurn action）或删表字段注明不可表
- **error 标记扩展**：response.failed/incomplete 落 TurnRecord.error 且 agent+generation 双 span 挂 native status（v0.2.2 已做）；error.type 属性、非流式失败响应与 ws `response.done(status=failed)` 的标记仍待做
- **SseAction::Usage 死载荷**：引擎忽略路径参数改 unit 变体（API 卫生）
- **req_buf 多次 parse 收敛**：session/user_input/model/end_user 已收敛为单次 parse（v0.2.2 已做）；剩 extract_messages 在空 session 兜底路径的二次 parse

## [0.2.2] - 2026-09-09

Langfuse 官方语义对齐：审计 P0 五项 + 全部语义歧义（AMB）清零。数据路径（SSE 单遍零拷贝）零改动、零新依赖；RustGate 守门累计五轮放行（§D 阶段 4 轮 + §E Round 2/3/4 复审，Round 4 PASS 零 BLOCK 零歧义）。

### 修复（Langfuse 语义）

- **P0-1 trace/span id 随机化**：确定性派生 id 在 Langfuse spanId upsert 下 85% 吞数、长会话巨型 trace——改 per-turn 随机 id（`replayed_turns_get_distinct_ids` 反向钉子），会话归组唯一靠 `langfuse.session.id` — `126272b`
- **P0-2 usage 互斥桶**：OpenAI inclusive input（cached_tokens 与 cache_write_tokens 均含于 input）生产侧派生为 exclusive 桶（input − cache_read − cache_creation，saturating clamp）；官方依据：OpenAI cookbook per-run spending controller（ordinary = input − cached − written）+ Langfuse 归一化表（flat `langfuse.observation.usage_details` "stored unchanged; values must already be exclusive"）— `126272b`、`4913756`
- **P0-3 observation.input/output**：agent 根 span 发官方内容键 `langfuse.observation.input/output`（空串省略，空输出噪声一并消灭）— `126272b`
- **P0-4 session 传播 + AMB-6**：`langfuse.session.id` 复制到 generation 子 span（observation 级过滤可见 usage）；双拼写收敛为官方单键（负向钉子防回潮）— `126272b`、`def4793`
- **P0-5 model.name**：`langfuse.observation.model.name` 挂 generation 子 span（TurnRecord.model_name，单次 req parse 填充，空则省略）— `126272b`
- **P1-6 langfuse.user.id**：agent+generation 双 span 非空发射（R2-1，死字段落地）— `def4793`
- **P1-7 ingestion header**：`x-langfuse-ingestion-version: 4`（v4 实时摄入）— `126272b`
- **R2-2 单次 parse**：logging 钩子 req_buf 单次 parse 共享给 session/user_input/model/end_user 提取器（C14 req 侧纪律）— `def4793`
- **AMB-1**：TurnUsage 文档改派生互斥桶语义（input_tokens 已减 cache，防 records/导出消费者二次扣减）+ usage_from_obj 减法注释引官方依据 — `4913756`
- **AMB-2**：usage_details 跳过 0 值条目（0 ≙ 未报告；Langfuse `total` 按在场桶求和，省略无损，消灭 `{"input":0,"output":0}` 空转噪声）— `4913756`
- **AMB-7**：errored turn 的 native OTLP status（code=2 + message）同时挂 agent 与 generation span（Langfuse level/statusMessage 按 span 映射，generation 过滤的错误视图不缺数据）— `9e025ec`
- **incidental**：chat ToolChunk name 首次命中即定（重发 name 的兼容网关不再拼出重复名）；删除零调用者 `extract_end_user`/`extract_model_name` — `680f2f0`

### 验证

CT104 实跑（RustGate 本机独立复跑数字一致）：lib **33/33**（+3 新钉子：usage_details_skips_zero_entries / errored_turn_marks_both_spans / chat_tool_name_first_hit_wins）、clippy `--all-targets -D warnings` clean、全量 debug 套件 **33+20 全绿**；release bench（数据路径与 v0.2.1 相同、零回归）：SSE 264KB 重组 435µs、anthropic 3000 帧/50 工具 4.42ms（CT104；bench 断言全过。v0.2.1 所引 372µs 为 RustGate 主机，数字机器相关）。

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
