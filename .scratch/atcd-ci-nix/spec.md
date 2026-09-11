# Spec: R8 — CI 迁移 nix 工具链 + runner 利用评估

Status: in-progress
Owner: PM 验收；CI-Nix agent 执行

## Requirement

ci.yml 与 dev 构建同源（nix 工具链，fenix 1.95.0），摆脱浮动 rust:1-bookworm；
充分利用可用 runner。现状 ci.yml 对 codex 依赖树必挂（缺 libssl-dev，native-tls
无法链接）。

## Specification（含依据）

- 首选形态：job 里 `nix develop -c cargo fmt/clippy/test`（与 devShell 完全同源，
  openssl/bindgen 问题随之消失）。
- runner 事实核查（先做）：gh api 查 Samuka007/agent-trace-gateway 与
  Vitus213/agent-trace-gateway 的 self-hosted runner 注册情况。sub2api-incus-1/2/3
  注册在 sub2api 仓库（label sub2api-pve/sub2api-incus），本仓库是否有 runner 是
  开放事实——没有就两条路报告：a) ubuntu-latest + 装 nix（DeterminateSystems 或
  官方 install 脚本）跑 nix develop；b) 给本仓库注册一个新 runner 容器（此路需
  PM 上报用户批准，agent 不得自行创建）。
- 现有 mihomo 代理配方（atcd-dev 已验证）适用于 runner 上跑 nix/cargo 的网络面。
- 不改 GitHub 仓库设置；只产出 workflow 文件与评估结论。

## Acceptance

- [ ] runner 可用性事实清单（gh api 输出留档）
- [ ] 新 ci.yml（或 overlay 文件）落盘：fmt/clippy/test 三步走 nix 工具链；若走
      self-hosted 路线标注所需 label 与准备步骤
- [ ] 本地静态验证：actionlint 或等价检查通过；nix develop 路径在 atcd-dev 容器
      实测可复现（模拟 CI 的命令序列跑通）
- [ ] 评估结论写回本 spec Comments：推荐路线 + 理由 + 需要用户批准的事项
