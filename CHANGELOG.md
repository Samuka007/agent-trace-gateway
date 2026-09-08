# Changelog

本项目的所有显著变更记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

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
