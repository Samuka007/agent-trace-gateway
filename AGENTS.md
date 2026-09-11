# AGENTS.md

atcd = agent-trace-gateway 的 Responses 反代守护进程（Rust）。本文件是 agent 进入本仓库的入口。

## Agent skills

### Issue tracker

Local markdown：`.scratch/<feature>/spec.md` + `.scratch/<feature>/issues/NN-<slug>.md`，一票一文件。见 `docs/agents/issue-tracker.md`。

### Triage labels

默认五个角色名：`needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix`。见 `docs/agents/triage-labels.md`。

### Domain docs

Single-context（根 `CONTEXT.md` + `docs/adr/`，按需惰性创建，缺席不报错）。见 `docs/agents/domain.md`。

## 项目文档指针（先读再做）

- `docs/atcd-design.md` — 统一客户端模型 + D1–D6 决策记录
- `docs/atcd-requirements.md` — 需求台账（R 编号）
- `docs/atcd-build-infra.md` — 构建/测试环境的事实、决策与指令台账：主机拓扑、凭据文件位置、禁区、镜像/代理配方都在这里
