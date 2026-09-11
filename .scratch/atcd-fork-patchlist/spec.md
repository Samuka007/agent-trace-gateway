# Spec: R9 — codex fork patch list 自动同步工具

Status: in-progress
Owner: PM 验收；PATCHLIST agent 执行

## Requirement

Samuka007/codex@atcd-libs 相对 openai/codex 的补丁集要有台账和可重复的同步工具：
上游更新后能自动重放补丁、验证、产出可审阅的分支/PR，而不是手工 rebase。

## Specification（含依据）

- 补丁集事实：`git log openai/codex主分支..atcd-libs` 即补丁提交集（fork remote
  git@github.com:Samuka007/codex.git，gh 已登录）。atcd 依赖它提供
  CodexResponsesMetadata 的字段可见性翻转（docs/atcd-requirements.md R1a/R8 行、
  Cargo.toml 注释有用途说明）。
- 工具形态：`scripts/fork-sync`（bash，set -euo pipefail）：
  1. 列出并导出补丁提交清单（含 message 与 diffstat）到 PATCHLIST.md；
  2. fetch 最新 upstream → 在其上重放补丁（rebase 或 format-patch/am，按补丁
     性质选择并在脚本头部写明）；
  3. 每次重放后跑 `cargo check -p codex-core -p codex-protocol`（fork 工作区）
     作为最低验证；
  4. 产出分支 `atcd-libs-sync-<date>`，**不自动 push**——push 与 PR 由 PM 核验
     后执行。
- 冲突处理：重放冲突即失败退出，列出冲突文件，不改写补丁内容。
- 仓库工作副本放 /home/nixos/workspace/scitrace/codex-fork-sync/（新建，
  .gitignore 于本仓库之外，不入 agent-trace-gateway git）。

## Acceptance

- [ ] PATCHLIST.md 生成：补丁清单 + 每个补丁的目的说明（从 commit message 与
      diff 归纳，与 atcd 用途对照）
- [ ] scripts/fork-sync 落盘并在当前 upstream HEAD 上完整跑通一次（产出
      atcd-libs-sync 分支 + 验证通过/冲突报告）
- [ ] 使用说明（触发时机、冲突时的操作）写入脚本头部注释或 docs/atcd-build-infra.md
- [ ] 不 push 不 force；一切产出可丢弃重建
