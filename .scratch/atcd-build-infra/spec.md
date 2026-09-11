# Spec: atcd-build-infra — 构建/测试 handoff 到远端 Incus 容器

Status: delivered（全部验收项通过，2026-09-11）
Owner: PM（Main 会话）；实现由 subagent 承担，PM 验收

## Requirement

atcd 的构建与测试不落在工作站上执行（本机磁盘容量小，构建负载影响交互使用）。
目标形态：dragonos-1288v3 上一个专用 Incus 容器，内含可复现的 NixOS 构建环境；
工作站改完代码 rsync 过去，容器内 `nix develop -c cargo build --bin atcd` 与
`nix develop -c cargo test` 全绿，证据（命令、退出码、日志路径）落盘可查。

非目标：不动 sub2api-incus-1/2/3 三台 runner；不改任何生产服务；GitHub Actions
CI 的改造另立需求；不解决源码本身的开发问题（那是主线的 R 台账）。

## Specification（含理由与被否方案）

1. **容器**：NixOS-on-Incus，新建于 dragonos-1288v3（用户指令 U14）。
   - 资源起点：16 vCPU / 20GiB 硬内存 / 100GiB 磁盘。理由：宿主可用内存
     ~26Gi（2026-09-11 实测），20GiB 上限保证不挤压宿主；宿主 56 核给 16 核
     足够并行 rustc；磁盘对齐 runner 先例（80Gi）并放宽到 100Gi（Rust debug
     target 树 10–20G 量级）。subagent 以宿主实时余量复核，不够就上报再调。
   - 被否：nixos-lxc（192.168.1.119）——曾按"可靠性优先"定为 D-B1，用户以
     "本机容量不大"改判到 incus 容器（U14 覆盖 D-B1）。
   - 被否：直接在宿主 dragonos 用户目录构建——污染宿主 home，且无资源隔离。
2. **网络**：China 网络，nix/cargo 需要 egress。优先复用宿主网桥里已验证的
   出口（先验：xray socks5 `10.0.100.191:10808`；备选：sub2api-mihomo
   `10.0.100.240`，以 sub2api-infra 的 runner 配方为真源）。由 subagent 实测，
   把实际可用配方写回 `docs/atcd-build-infra.md`。
3. **devShell**：flake.nix 已改为自洽——fenix `fromToolchainFile` 直接吃
   codex input 的 `codex-rs/rust-toolchain.toml`（1.95.0），openssl+pkg-config
   走 rust-openssl "Automatic" 路径，bindgen 用 `rustPlatform.bindgenHook`。
   工作站已实证：rustc 1.95.0 / openssl 3.6.3 via pkg-config / LIBCLANG_PATH
   与 BINDGEN_EXTRA_CLANG_ARGS 就位。容器内应零额外环境变量直接可用。
4. **同步**：rsync over ssh（工作站 worktree → 容器），排除 `.git`、`target`。
   工作站是唯一真源，容器是构建执行器，不做双向同步。

## Acceptance

- [x] A1 容器存在，资源限额实测生效（`incus list` + `incus config show` 输出留档）
- [x] A2 容器内 `nix develop -c rustc --version` = 1.95.0；`pkg-config --modversion openssl` 有输出；`$LIBCLANG_PATH` 非空
- [x] A3 容器内 `nix develop -c cargo build --bin atcd` 退出码 0，完整日志落盘（产物 76MB，子命令探针留证）
- [x] A4 容器内 `nix develop -c cargo test` 退出码 0（45 passed / 0 failed，日志落盘）
- [x] A5 三台 runner 容器状态前后不变（`incus list` 对照留档）
- [x] A6 实际可用的 egress 配方写进 `docs/atcd-build-infra.md`
