# Spec: R8 — CI 迁移 nix 工具链 + runner 利用评估

Status: delivered（CI-Nix agent，2026-09-11，待 PM 验收）
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

- [x] runner 可用性事实清单（gh api 输出留档）
- [x] 新 ci.yml（或 overlay 文件）落盘：fmt/clippy/test 三步走 nix 工具链；若走
      self-hosted 路线标注所需 label 与准备步骤
- [x] 本地静态验证：actionlint 或等价检查通过；nix develop 路径在 atcd-dev 容器
      实测可复现（模拟 CI 的命令序列跑通）
- [x] 评估结论写回本 spec Comments：推荐路线 + 理由 + 需要用户批准的事项

## Comments（CI-Nix agent，2026-09-11）

### 1. Runner 事实清单（gh api 实测输出留档）

查询时间 2026-09-11，本地 gh（Samuka007 token）。

**Samuka007/agent-trace-gateway（fork，当前工作仓）**

```
$ gh api repos/Samuka007/agent-trace-gateway/actions/runners
{"runners":[],"total_count":0}
```

→ fork 无任何 self-hosted runner。GitHub-hosted 可用（public repo，托管分钟免费）。

**Vitus213/agent-trace-gateway（upstream，origin）**

```
$ gh api repos/Vitus213/agent-trace-gateway/actions/runners
{"message":"You must have repository read permissions or have the repository
runners fine-grained permission.","status":"403"}

$ gh api repos/Vitus213/agent-trace-gateway   # 本地 token 权限
{"permissions":{"admin":false,"maintain":false,"pull":true,"push":false,...}}

$ gh api repos/Vitus213/agent-trace-gateway/actions/runs
{"total_count":0}
```

→ runners 列表 API 需 admin 权限（本地 token 仅 pull）无法直接枚举；但该仓
**workflow runs 总数为 0**——从未执行过任何 CI，无 runner 使用痕迹。

**Alle-Group/sub2api（对照，sub2api fleet）**

```json
{"runners":[
  {"busy":false,"labels":["self-hosted","Linux","X64","sub2api-pve","sub2api-incus"],"name":"sub2api-incus-1","status":"online"},
  {"busy":false,"labels":["self-hosted","Linux","X64","sub2api-pve","sub2api-incus"],"name":"sub2api-incus-2","status":"online"},
  {"busy":false,"labels":["self-hosted","Linux","X64","sub2api-pve","sub2api-incus"],"name":"sub2api-incus-3","status":"online"},
  {"busy":false,"labels":["self-hosted","Linux","X64","sub2api-pve"],"name":"sub2api-pve-ct-104","status":"offline"},
  {"busy":false,"labels":["self-hosted","Linux","X64","sub2api-pve"],"name":"sub2api-pve-ct-104-2","status":"offline"}
],"total_count":5}
```

→ Alle-Group 是 Organization：这 5 个 runner 是 **org-scoped**，只服务组织内
仓库。agent-trace-gateway 两个 remote 都不在该 org 下，**跨仓复用不可行**。

### 2. 交付物与验证记录

- 新 `.github/workflows/ci.yml` 落盘：test job = `ubuntu-latest` +
  `DeterminateSystems/nix-installer-action@v23` + `Swatinem/rust-cache@v2` +
  `nix develop -c cargo fmt/clippy/test` 三步；image job 原样保留（Dockerfile
  路径）；self-hosted 变体以注释模板标注（label `atg-builder` + 准备步骤）。
- 静态检查：`actionlint 1.7.12 -shellcheck(shellcheck 0.11.0)` → **0 findings**。
- atcd-dev 容器实测（root@10.0.100.244:/opt/atcd，nix 2.34.8；rsync 前置校验
  flake.nix/Cargo.lock md5 与 worktree 一致，模拟 CI 前 rsync 覆盖最新源码）：
  1. 首轮序列 `nix develop -c cargo fmt --check` 51s 内执行并**正确退出 1**：
     atcd 源码树（src/bin/atcd.rs、src/atcd/*.rs 等）当前有约 487 行 rustfmt
     diff。这**不是工具链问题**——fenix 1.95.0 工具链正常解析执行，fmt 门禁
     抓到的是仓库现状（worktree 本身同样 fmt-unclean）。合入新 ci.yml 前必须
     先 `cargo fmt`，否则首跑红在 fmt 门。
  2. 容器副本（一次性 rsync 镜像，未动共享 worktree）内 `cargo fmt` 后全序列
     重跑：`fmt --check` / `clippy --all-targets -- -D warnings` / `cargo test`
     **全绿**（CI-SEQUENCE-ALL-GREEN）。实测：fmt --check 通过；clippy 热缓存
     4.4s 完成；test 构建热缓存 1m37s、27 单测 + 16 个集成测试文件全过
     （0.7s-2.2s/文件，0 failed）。

**容器副本内为达成全绿所做的人工修复（worktree 尚未包含，合入 ci.yml 前需在
worktree 重放，共 5 处、全部机械）：**

1. `cargo fmt`（全树 ~487 行 rustfmt 归一化）。
2. `src/atcd/oauth.rs`：编号列表与新段之间补一行空 `//!`（clippy
   doc_lazy_continuation；取空行分段而非 clippy 建议的缩进续接，保段落语义）。
3. `src/atcd/proxy.rs:71`：`hex::encode(h.finalize())[..16].to_string()` →
   `&hex::encode(h.finalize())[..16]`（clippy to_string_in_format_args）。
4. `src/trace/unpack.rs`：`"openai_responses"` 与 `"anthropic_messages"` 两个
   match 臂的外层 `if` 收拢为臂 guard（clippy collapsible_match ×2）。

（4 处 clippy 修复已同时在容器镜像验证 clippy/test 全绿。）

### 3. 评估结论

**二轮实跑修复（run 34625046867，2026-09-11）**：真源 ci.yml 已由 PM lane 迭代
（nothing-but-nix 大盘 + `CARGO_TARGET_DIR=/nix/build/target` +
`build-dir = /nix/build` + rust-cache 跟踪该目录）。磁盘满（root 卷 2GB safe
haven）消失后暴露下一层：`/nix/build` root 属主，runner 用户的 cargo 写入
EACCES（os error 13）。修复 = nothing-but-nix 步加文档化输入
`nix-permission-edict: true`（action 挂载后 `chown -R runner /nix`；本 job 里
/nix/build 唯一写者就是 runner 用户 cargo，无沙箱 derivation 构建落盘，所有权
安全）。actionlint+shellcheck 复验通过。

**推荐路线：GitHub-hosted（ubuntu-latest）+ nix-installer-action + `nix develop -c`**（本次落盘形态）

理由：
1. **零审批、即刻可用**：两仓都无 self-hosted runner；GitHub-hosted 对 public
   repo 免费，fork（Samuka007，当前 push 目标）直接可跑。
2. **网路面零配置**：GH runner 在 Azure US，github.com / crates.io /
   cache.nixos.org 直连。atcd-dev 的 mihomo/TUNA/rsproxy 配方是 China-network
   特化（docs/atcd-build-infra.md），hosted 上不需要、也不应带入。
3. **与 dev 完全同源**：工具链由 flake.nix devShell 单点定义（fenix
   fromToolchainFile 钉 codex rust-toolchain.toml = 1.95.0，含 openssl +
   pkg-config + bindgenHook），CI 不再维护第二份工具链清单；openssl-sys /
   boring-sys 两类链接问题随 devShell 消失（工作站与容器双实证在案）。
4. **速度可接受**：首跑 nix 层（nixpkgs + fenix fetch）约 3-5min + cargo 全量
   约 15-25min（约 350 crates + BoringSSL）；rust-cache 命中后增量约 5-10min。

**self-hosted 备选路线（需用户批准，当前非必需）**

- 现有 fleet org-scoped 不可跨仓复用（见事实清单）。
- 若批准为本仓库注册新 runner：建议 PVE LXC 容器，label `atg-builder`，注册到
  Samuka007/agent-trace-gateway。准备步骤：容器内预装 nix（flakes enabled）+
  atcd-dev 同款 egress 配方（nix channel→TUNA、cargo→rsproxy-sparse、mihomo
  `socks5h://10.0.100.240:10809` 兜底 github）+ runner 服务常驻。workflow 改动
  仅 `runs-on: [self-hosted, atg-builder]` 并删去 nix-installer 步骤——ci.yml
  内已留注释模板，其余步骤一字不动。
- 收益：不吃托管分钟（若仓转 private 会计费才显著）、nix store + target 热缓存
  使增量构建约 2-5min。当前 public 免费 + rust-cache 已够用。

**docker build job 的处理（设计建议）**

- 短期（本次落盘形态）：image job 原样保留——deploy/Dockerfile 自带 apt 工具链
  清单（cmake/clang/go/perl/pkg-config），pre-codex 树上 84 次 success 实证。
  [INFERENCE] 风险标记：codex 依赖树合入 main 后，Dockerfile builder 会撞同一
  openssl-sys 链接问题（rust:1-bookworm 无 libssl-dev），届时在 apt 行补
  `libssl-dev` 即可（与 devShell 的 openssl+pkg-config 等价）。
- 中期建议：image job 切 nix 原生——`nix build .#dockerImage-v1`（flake 输出已
  备，streamLayeredImage 直出 image tar）→ `docker load` → retag/push ghcr
  （v1→latest 映射）。收益：镜像与 CI/dev 工具链同源、digest 可复现
  （created=1970）、ISA 分层 tag（latest/v2/v3）自然落位。注意输出 tar 无
  repo/tag 元数据，push 前需 retag 或改用 skopeo copy。

### 4. 需要用户批准 / 决策的事项

1. （可选，非阻塞）为本仓库注册 self-hosted runner `atg-builder`——当前
   GitHub-hosted 路线无需任何批准即可运行。
2. push 新 ci.yml 时**须先 `cargo fmt`**（worktree 现状 fmt-unclean）；push 动作
   本身由 PM/用户执行（本 agent 受令不 commit 不 push）。
3. （中期，另行立项）image job 切 nix 原生构建；Dockerfile 在 codex 树合入
   main 前补 `libssl-dev`。
