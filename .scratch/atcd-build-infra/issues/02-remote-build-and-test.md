# 02 — atcd-dev 容器内全量构建 + 测试

Status: resolved
Blocked by: 01

## 目标

工作站 atcd worktree（含未提交改动）同步进 atcd-dev 容器，容器内完成
`cargo build --bin atcd` 与 `cargo test`，全绿并留证。这是昨日会话中断点
（"源码已编过、卡在最后链接"）的正式收尾。

## 环境与前提（指针）

- 容器由 01 号票供给（IP、限额、egress 配方以 01 号票验收记录为准）。
- 源真源：工作站 `/home/nixos/workspace/scitrace/agent-trace-gateway/.worktrees/atcd`。
- devShell 已自洽（spec §3）：容器内不得设置任何 `OPENSSL_*` /
  `LIBCLANG_PATH` 环境变量，一切由 `nix develop` 提供。
- 内存预算：容器硬顶 20GiB；若链接 OOM，优先降 `-j`（记录实际值），不得
  去改工作站。

## Acceptance（对应 spec A3/A4）

- [x] rsync 后容器内 `git status --short` 与工作站 diff 清单一致（Cargo.toml / Cargo.lock / src/atcd/rewrite.rs / flake.nix / flake.lock / docs / AGENTS.md / .scratch）
- [x] `nix develop -c cargo build --bin atcd` 退出码 0，产物 `target/debug/atcd` 存在，`--version` 或 `--help` 可执行
- [x] `nix develop -c cargo test` 退出码 0
- [x] 两次运行的完整日志落盘（路径记录于本票 Comments），构建时长记录在案
- [x] `-j` 实际取值与内存峰值（如有观测）记录在案

## 报告要求

同 01 号票：原始命令、阶段、资源状态、根因、备选、持久化输出路径。

## Comments

### 2026-09-11 收尾复测（rewrite.rs 测试签名修复后）— BuildVerify

**前置**：工作站已修复 `src/atcd/rewrite.rs` 测试签名（D6 生产签名：
`codex_envelope_body(body, &HashMap)` 2 参；garbage 测试断言 `None`）。前次
中断 agent 未在容器/本机留下任何 cargo/memsampler/nix develop 进程（pgrep
核实，仅匹配到检查命令自身）。

**同步**（`rsync -az --checksum --delete --exclude={.git,target,.cargo}
/…/.worktrees/atcd/ root@10.0.100.244:/opt/atcd/`，退出码 0）：

- dry-run 显示**源码零差异**——前次 agent 已把修复后 rewrite.rs 同步进容器
  （evidence/02/md5-*-final.txt 与此一致）；本次实传仅 evidence 目录新增文件
  ，并按 `--delete` 清掉容器侧残留 `atcd.db`、`test-rerun.log`。
- 容器无 `.git`（rsync 设计排除），等价验证改为内容级对账：rsync
  `--checksum` 全量比对 + 关键 5 文件（Cargo.toml / Cargo.lock /
  src/atcd/rewrite.rs / flake.nix / flake.lock）双侧 `md5sum` 逐字节一致
  （workstation 21ddb868… / 48ad17b3… / 1579f30c… / 4dcabf75… / a2c46d27…，
  容器侧完全相同）。与工作站 `git status --short` 的 diff 清单（5 M + 4 ??）
  一致。

**测试**（容器内 `cd /opt/atcd && nix develop -c cargo test`，2026-09-11
17:08:12–17:08:53 +08:00）：**退出码 0，耗时 41s**（增量，仅重编受
rewrite.rs 影响的 crate）。完整日志：`evidence/02/test-fixed.log`（172 行）。
逐条结果：

| 套件 | 结果 |
|---|---|
| unittests src/lib.rs | 27 passed; 0 failed（0.12s） |
| unittests src/bin/atcd.rs | 0 tests, ok |
| unittests src/bin/gateway.rs | 0 tests, ok |
| tests/（17 个集成测试文件） | 18 passed; 0 failed（最长 export_fail_open 2.19s） |
| Doc-tests | 0 tests, ok |

合计 **45 passed / 0 failed / 0 ignored**。修复点 `atcd::rewrite::tests` 8 条
全部 ok，含 `envelope_body_none_on_garbage`（garbage → None 断言）与
`envelope_body_synthesizes_codex_shape`（2 参新签名）。

**A3 补全（产物）**（`evidence/02/atcd-cli-probe.txt`，本日复测覆盖）：
`target/debug/atcd` 存在（76,486,208 B，2 硬链接，Sep 11 16:57）。CLI 为纯
子命令式（无顶层 --version/--help，未知参数 exit 2 并列出
serve|import|accounts|enable|disable|login 用法）；正向执行证明：`atcd
accounts` exit 0 输出账户表头，`atcd`（默认 serve）打印
`atcd serving 127.0.0.1:8400 → …` 横幅。

**内存观测**（测试阶段，`free -m` 前后两次快照，见
`evidence/02/resource-snapshot.txt`）：before used=50 MiB / after used=51 MiB
（cap 20480 MiB），测试阶段内存压力可忽略；构建阶段峰值 19.35 GiB / 96.5%
沿用上一条记录。`-j` 未显式设置（ninja/cargo 默认按核数 16），本轮测试增量
编译未触及链接 OOM 风险路径。

**A3/A4 验收状态**：A3 ✅（build 退出码 0 + 产物存在可执行 + 双日志落盘
`build.log`/`build-after-fix.log`）、A4 ✅（test 退出码 0 + 完整日志
`test-fixed.log` 落盘）。02 号票全部 acceptance 达成。
