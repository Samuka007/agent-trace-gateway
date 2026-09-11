# atcd 构建与测试交接 — 指令、事实与决策台账

本文档只有一个目的：把本次交接中用户的每一条指令、每一个已验证的事实、每一个决策（含理由和代价）写成文字。
这样上下文被压缩、会话被更换后，后续执行者不需要重新推理，也不会来回推翻已经定下的东西。

维护规则：新事实进 §2，新决策进 §3，还没定的事进 §4。决策被推翻时，不删条目，追加"推翻记录"并写明新证据。

---

## 1. 用户指令台账（2026-09-11 会话，逐条）

| # | 指令 | 操作含义 |
|---|------|---------|
| U1 | 从 `/home/nixos/.zcode/cli/log/zcode-2026-09-10.jsonl` 继续，构建和测试 handoff 到 pve 或 incus，infra 查记忆 | 恢复昨日 atcd 会话的现场（worktree `feat/atcd-minimal-proxy`，源码已编过、卡在最终链接被停），把构建/测试搬到别的机器 |
| U2 | `nixos-pve` 是 LXC 不是 PVE 机器；PVE 机器就叫 `pve`；用 `ls ssh://` 查主机 | 主机别名的语义：`nixos-pve`=PVE 上的 NixOS 客体机，`pve`=PVE 宿主本身 |
| U3 | incus 在 `dragonos-1288v3` 上；三台 sub2api-incus-* 是 GitHub runner 构建机，**不能动** | 三台 runner 容器是只读禁区；Incus 服务端在 dragonos-1288v3 |
| U4 | 要用记忆，且用 `xd://recall`，不是 `xd://mcp__hindsight_recall` | 记忆工具用内置 recall 设备 |
| U5 | SSH 配置在 `./.omp/ssh.json`，不在 SciBuddy 下 | 记忆里的旧路径错了；对记忆保持怀疑、现场验证 |
| U6 | 提供 dragonos-1288v3 的 sudo 密码，写入 `dragonos-1288v3-sudo-key.secrets` | dragonos 账号无免密 sudo；需要特权操作时用该文件（**密码本身不得写进任何文档**） |
| U7 | 补全 devShell，使用最佳实践配置环境；镜像/代理与资源的权衡我来做 | flake 的 devShell 是上次硬编码 store path 的根因，要修成自洽的 |
| U8 | 多查文档；不要"以前怎么做现在就怎么做"；质疑每个选择；决策逻辑落盘，避免循环推翻 | 每个技术选择要有一手来源支撑，本文档即落盘载体 |
| U9 | 遵循 POMDP 范式：记忆只当先验方向，快速用真源检验，再做 solid 决策 | 记忆→假设→真源验证→决策，四步走 |
| U10 | 文档语言：费曼式学术风——通俗易懂、不造作、保持严谨 | 本文档的文风要求 |
| U11 | 指出：nix-based Rust 开发环境的论述没查过文档 | 补查 nixpkgs manual / mkShell / bindgenHook / rust-openssl / clang-sys 等一手来源 |
| U12 | 指出：明明有 fenix 之类的 flake（还有 oxalica rust-overlay） | devShell 的工具链来源要正式比较 fenix / rust-overlay / nixpkgs rustc，不能默认拿 nixpkgs 的 |
| U13 | 一条条来；先把所有问题和决策文字记录下来（即本文档） | 先落盘再干活；串行处理未决项 |
| U14 | 挪到远端开 incus 容器开发，本机容量不大 | 覆盖 D-B1：构建/测试环境改为 dragonos-1288v3 上新建 Incus 容器，工作站不做重活 |
| U15 | 采用项目管理姿态：细节 handoff 给 subagent、难题给 oracle、需求走跟踪框架、决策记录含真源信息、用 matt 系列 skill 跟踪、产物用 reviewer 验收、xd:// 工具先 Read 再用 | 追踪框架落地为 `.scratch/atcd-build-infra/`（spec + 工单）；matt 系列 skill 已初始化（AGENTS.md + docs/agents/，Local markdown tracker + 默认 triage 标签，用户确认） |

---

## 2. 事实台账（全部为本会话验证过的；标注"先验"的除外）

### 2.1 上游任务现场（来自 zcode 日志 + rollout + git）

- 昨日 zcode 会话最终状态（最后一条助手消息原文）：worktree `.worktrees/atcd`（分支 `feat/atcd-minimal-proxy`）。
- 已提交链：`a1e6650` 原型 → `1466663` 身份层复用 codex → `23f90d9` R1/R2/R3 → `e1d0514` R3a 台账 → `5afea50` 首次真实下游流量捕获 → `8fe1e59` R3a body 信封合成 → `9894a72` root_turn_id/context_window_id 跨轮稳定 → `06430bd` 设计文档 + D6 语义映射收窄（HEAD）。
- 未提交改动（3 文件）：`Cargo.toml`（rusqlite 0.32→0.39；rama-macros 钉 `=0.3.0-alpha.4`；新增 `[patch.crates-io]` 把 tokio-tungstenite/tungstenite 指到 openai-oss-forks 的 git rev）、`Cargo.lock`（大改）、`src/atcd/rewrite.rs`（铸造函数切换到 codex 的 `CodexResponsesMetadata`；第三方 body 信封按 D6 收缩为纯源信息）。
- 上次构建配方 `/tmp/build_atcd.sh`：`nix develop -c cargo build --bin atcd`，外加**硬编码工作站 /nix/store 路径**的 `OPENSSL_LIB_DIR`/`OPENSSL_INCLUDE_DIR`/`LIBCLANG_PATH`。这套路径在别的机器上不存在——这是 devShell 必须补全的直接原因。
- 设计文档已存在：`docs/atcd-design.md`（含 D1–D6 决策）、`docs/atcd-requirements.md`（需求台账）；金样本 `scripts/fixtures/codex_exec_0.153.4.*`。
- 仓库**没有** `rust-toolchain.toml`，也**没有** `.cargo/` 目录（已验证）。
- 依赖树事实（`cargo tree` 实测）：
  - `openssl-sys 0.9.117` 仅经由 `native-tls ← codex-http-client ← codex-login ← … ← codex-core(Samuka007/codex@atcd-libs#9f95fe18)` 进入，**未开启 vendored feature**（features 树里只有 "default"）→ 适用 rust-openssl 文档的 "Automatic" 路径：Unix 上用 pkg-config 发现系统 OpenSSL。
  - `bindgen 0.72.1` 是 `boring-sys 4.22.0`（BoringSSL）的 build-dependency，链路 `pingora → pingora-boringssl → boring → boring-sys` → bindgen 在依赖树里，devShell 必须解决 libclang 发现问题。
- 仓库自述（Cargo.toml 注释）：曾撤下 codex-login 是因其 native-tls/openssl 在 nix 构境无法链接；未提交改动重新引入 codex 系依赖（字段正确性改由复用代码保证）。

### 2.2 基础设施（本会话 SSH 实测）

- 别名与地址（来源：`ls ssh://` + `~/.ssh/config` + `./.omp/ssh.json`，其中 `./` = `/home/nixos/workspace/scitrace`）：
  - `pve` = root@192.168.1.107 —— PVE 宿主，Debian 12，hostname `dragonos-stack`，20 核 / 62Gi 内存（可用 16Gi）/ 53G 空闲盘，**无 nix**。
  - `nixos-pve`（OMP 别名 `nixos-lxc`）= samuka@192.168.1.119 —— PVE 上的 NixOS 客体机（hostname `pve-nixos`；磁盘 LVM 名 `pve-vm--119--disk--0`），nix 2.34.7，flakes 已启用（experimental-features 含 `flakes nix-command fetch-tree`），18 核 / 20Gi 内存（可用 ~9.5Gi）/ 169G 空闲盘，rsync/git 可用。
  - `dragonos-1288v3` = dragonos@100.64.0.17:2222 —— Incus 宿主，NixOS，56 核 / 125Gi 内存（可用 ~26Gi）/ 315G 空闲盘，nix + incus 6.0.6 可用；**dragonos 账号无免密 sudo**（U6 的密码文件解决）。
  - `sub2api-incus-1/2/3` = 10.0.100.241-243，GitHub Actions runner（NixOS 声明式配置，8Gi 硬内存上限）——**禁区，不动**（U3）。
- 网络（nixos-lxc 实测）：github.com 200、static.crates.io 200、index.crates.io 200 —— **直连即可，无需镜像/代理**。
- ~~先验（记忆，未在本会话复验）：Xray fetch egress `10.0.100.191:10808`~~ **已复核并被覆盖**：191 是自建 xray 单节点（103.155.37.8 VLESS-reality），实测吞吐 ~1.13MB/s，降为备用；主出口是机场直喂的 `sub2api-mihomo` 10.0.100.240（见 §2.4）。记忆里"curl.conf 仍指 191"是 2026-08-28 迁移前旧状态，同日已修为 240。
- OMP ssh 注册表 `./.omp/ssh.json` 当前只有 sub2api-incus-1/2/3 和 nixos-lxc 别名；`pve`/`dragonos-1288v3` 等来自 `~/.ssh/config`。

### 2.3 一手文档结论（已读原文）

- rust-openssl（openssl crate lib.rs "Building" 一节）：非 vendored 时 Unix 走 **pkg-config 自动发现**；`OPENSSL_DIR`/`OPENSSL_LIB_DIR`/`OPENSSL_INCLUDE_DIR` 是**覆盖自动发现的手动兜底**；`OPENSSL_NO_VENDOR=1` 可在 vendored feature 开启时强制走系统库。
- clang-sys README：bindgen 找 libclang 的首选环境变量是 `LIBCLANG_PATH`（目录或 libclang.so 全路径）。
- nixpkgs `pkgs/build-support/rust/hooks/rust-bindgen-hook.sh`（真源）：`rustPlatform.bindgenHook` 会自动导出 `LIBCLANG_PATH=<clang.cc lib>/lib` 和 `BINDGEN_EXTRA_CLANG_ARGS`（取自 clang wrapper 的 nix-support 文件）——即 nixpkgs 的标准解法是**把这个 hook 放进 nativeBuildInputs**，而不是手写 shellHook。
- nixpkgs manual 含 `sec-pkgs-mkShell` 一节（已定位在 manual 全文 ~16420 行）；mkShell 源码路径尚未定位到（旧路径 404，待查）——见 §4-2。

---

### 2.4 atcd-dev 容器供给（01 号票执行事实，2026-09-11）

全部为执行中实测。证据文件：`.scratch/atcd-build-infra/evidence/01/`
（configuration.nix、atcd-dev-config-show.txt、atcd-dev-devices-state.txt、
incus-list-before/after.csv、egress-probe.sh、devshell-env.txt）。

- 容器：`atcd-dev`（dragonos-1288v3 / incus，CONTAINER，RUNNING），镜像
  `images:nixos/26.05`（Nixos 26.05 amd64 20260911_01:03，与 runner 同族）。
  工单原文写 26.11：按"复用 runner 已验证镜像路径"偏差执行并记录；且 26.11
  镜像当前不存在（NixOS 发布节奏 .05/.11，今天 9 月），偏差有客观依据。
- 资源（与设计一致，`incus config show` 全文留档工单 Comments）：limits.cpu=16、
  limits.memory=20GiB（hard、swap=false；容器内 state 实测 total=21474836480、
  swap_usage=0）、limits.processes=8192、security.nesting=true、root 100GiB
  （pool default；state 实测 total=107374182400）、boot.autostart=true。profile 挂
  default（nixos-builder 同款；实例级 limits 覆盖 profile 值）。
- 网络：eth0 挂 incus_bridge_0，nic 设备属性静态预留 `ipv4.address=10.0.100.244`
  （runner 同款固定 IP 机制，dnsmasq 预约）；guest 侧 systemd-networkd DHCP 拿到该 IP。
  工作站经 tailscale0 table-52 子网路由直达 10.0.100.0/24，`ssh root@10.0.100.244`
  免密直连成功（root+samuka 均注入工作站 ed25519 公钥；PasswordAuthentication=false，
  samuka 免密 sudo）。重启后 IP/配置/工具链缓存全部保持。
- CLI 坑：`incus launch --device eth0,type=nic,...` 在本机 incus 6.0.6 会把 type 值
  吞成 `nic,network=...`（Failed creating instance record）。可靠做法：`incus create`
  + `incus config device add atcd-dev eth0 nic network=... ipv4.address=...` 分步执行。
- guest 声明配置：flakes（nix-command flakes）+ git/rsync/curl/vim/htop + openssh +
  Asia/Shanghai；源文件留档 evidence/01/configuration.nix，与容器内
  /etc/nixos/configuration.nix 同字节。nixos-rebuild 需经 login shell
  （`incus exec -- sh -lc`）：裸 exec 无 NIX_PATH，报 `nixpkgs/nixos not found`。
- egress 实测（容器内 curl，2026-09-11）：
  - github.com 根路径 200 (0.87s)；但 **github.com/*/archive/*.tar.gz 直连不通**
    （45s 0 字节超时）——nix flake 抓取路径，必须走代理。
  - codeload.github.com tarball 200，1.1–3.9 MB/s（归档重定向目标）。
  - cache.nixos.org 200，narinfo 0.67s；channels.nixos.org 200（channel 拉取 109s）。
  - static.rust-lang.org 200（channel-rust-1.95.0.toml 4.6s，fenix 工具链源）。
  - index.crates.io 200；static.crates.io 根 403（正常），crate 下载 206 但 ~78KB/s（慢）。
  - 代理双备（均实测 200）：**主出口 mihomo 10.0.100.240**——socks5h://240:10809
    （github 0.83s；53MB nixpkgs tarball 实测 3.18 MB/s）、http://240:10808
    （0.84s，2.12 MB/s）。白名单双重门已含 10.0.100.244/32（宿主 mihomo.nix 声明 +
    容器 config.yaml lan-allowed-ips；Main 改声明源，本 agent 起 mihomo-fetch.service
    生效并验证）。备用 xray 10.0.100.191:10808（github 2.6s / archive 3.3s，可用但慢）。
- **最终配方（镜源优先，proxy 只兜底，Main 转达用户裁定）**：
  1. nix channel → TUNA `https://mirrors.tuna.tsinghua.edu.cn/nix-channels/nixos-26.05`（已切，update 成功）；
  2. cargo → rsproxy（`~/.cargo/config.toml` replace-with rsproxy-sparse；
     configuration.nix activationScripts 声明式下发 root+samuka）；
  3. `networking.proxy.default = "socks5h://10.0.100.240:10809"`；noProxy =
     `127.0.0.1,localhost,10.0.100.0/24,cache.nixos.org,channels.nixos.org,static.rust-lang.org,index.crates.io,static.crates.io,mirrors.tuna.tsinghua.edu.cn,rsproxy.cn`。
     github.com 与 codeload.github.com 都走代理：前者直连被墙，后者与同族以防抖动。
     socks5h = 代理端解析 DNS，避 GFW 污染。
- 工具链冒烟（验收项）：工作站 worktree 的 flake.nix+flake.lock rsync 到容器
  /root/atcd-devshell，冷启动 `nix develop -c rustc --version` 全链 14.6 分钟走通：
  flake inputs（fenix、codex@9f95fe18 等）经代理抓取 → fenix 拉
  channel-rust-1.95.0.toml + 全套 1.95.0 组件（static.rust-lang.org 直连）→
  devShell 闭包（cache.nixos.org 直连）。输出 `rustc 1.95.0 (59807616e 2026-04-14)`、
  `cargo 1.95.0 (f2d3ce0bd 2026-03-21)`、`pkg-config --modversion openssl` = 3.6.3、
  LIBCLANG_PATH=clang-21.1.8-lib/lib、BINDGEN_EXTRA_CLANG_ARGS 由 bindgenHook 注入
  ——与工作站 D-B2 实证完全一致。热复用秒回；最终配置（mihomo 主出口）后复验通过，
  重启后再验通过。devShell 闭包落盘约 6.5GiB（root 100GiB 充裕）。

## 3. 决策台账（含理由与代价）

### D-B1（已被 U14 覆盖）：~~构建/测试 handoff 目标 = nixos-lxc（pve-nixos）~~

**覆盖记录（2026-09-11）**：用户以"本机容量不大"为由改判——新建 dragonos-1288v3 上的
Incus 容器承担构建/测试（U14）。原选择（nixos-lxc，理由：零网络配置、可靠性优先）及
以下原始权衡保留作为历史，若未来容器方案受阻可回溯：

- 权衡（U7 授权我决策）：
  - nixos-lxc：18 核 / 可用 ~9.5Gi / 169G 盘；nix 2.34.7 + flakes 就绪；**github/crates.io 直连，零网络配置**（§2.2 实测）。
  - dragonos-1288v3（incus 宿主）：56 核 / 可用 ~26Gi / 315G 盘，资源碾压；但中国网络环境，nix 与 cargo 都要镜像/代理加速，配置面大、故障点多（nix daemon 级代理或 override-input 镜像替换，都需要额外工程）。三台 runner 容器不可动（U3），新建容器要 sudo（有密码但没必要）。
- 结论：**可靠性优先，选 nixos-lxc**。18 核够用（上次工作站上除链接外全量编译已通过；`profile.dev debug = 0` 也会显著压低链接内存）；用 `-j` 上限控内存峰值（数值待首跑定，§4-4）。
- 代价：迭代速度不如 dragonos；若 OOM 或过慢，回退方案是 dragonos + 镜像/代理（先验入口 §2.2）。
- 推翻条件：LXC 上链接 OOM 且调 `-j`/`debug` 救不回来；或单次全量构建时间不可接受。

### D-B2（已定并实证）：devShell 补全 —— fenix 跟随 codex 的 toolchain file

**最终设计（含用户指正）**：工具链不是硬编码版本号，而是把 codex 仓库作为 flake
input（`flake = false` 只取源码树），fenix `fromToolchainFile` 直接吃
`codex-rs/rust-toolchain.toml`（channel 1.95.0，clippy/rustfmt/rust-src）。
codex input 与 Cargo.lock 的 git 依赖钉同一分支同一 rev（`9f95fe18`，flake.lock
与 Cargo.lock 两处 pin，升级必须一起动）。

**真源依据**：
- codex-rs 自己钉了 `rust-toolchain.toml → 1.95.0`（读自
  `~/.cargo/git/checkouts/codex-db571c5dd4d8f153/9f95fe1/codex-rs/`）——复用其源码
  就用其钉的编译器（用户哲学："行为的绝对正确由绝对复用源码保证"）。
- nixpkgs `rust-bindgen-hook.sh`（真源已读）：bindgenHook 自动导出 LIBCLANG_PATH
  与 BINDGEN_EXTRA_CLANG_ARGS → 采用，不手写。
- rust-openssl lib.rs "Building"（真源已读）：非 vendored 时 Unix 走 pkg-config
  自动发现 → devShell 放 `openssl`+`pkg-config`，任何 OPENSSL_* 变量都不要。
  依赖树实测：openssl-sys 0.9.117 仅经 codex native-tls 进入、未 vendored；
  bindgen 0.72.1 经 pingora→boring-sys 进入。
- CI 对照：`.github/workflows/ci.yml` 用 `rust:1-bookworm` 浮动最新 stable，
  不用 nix → 与 dev 的完美同源不可能，且该 ci.yml 尚未适配 codex 依赖树
  （缺 libssl-dev，native-tls 会挂）——列为本票范围外的开放问题。

**实证（工作站，2026-09-11）**：`nix develop -c` 下 rustc 1.95.0 (59807616e
2026-04-14)、cargo 1.95.0、clippy 0.1.95、`pkg-config --modversion openssl` =
3.6.3、LIBCLANG_PATH 指向 clang-21.1.8-lib/lib、BINDGEN_EXTRA_CLANG_ARGS 已设。
硬编码 store path 的旧配方 `/tmp/build_atcd.sh` 由此作废。

**哈希钉法**：sha256 钉 channel manifest（fenix 纯 eval 要求）。第一次用本地
curl 计算的哈希与 nix daemon 实际抓取不符（内容有差异，以 daemon 字节为准），
按设计大声失败后改为 `sha256-gh/xTkxKHL4eiRXzWv8KP7vfjSk61Iq48x47BEDFgfk=`。
fork 升 toolchain 时此哈希需同步更新——错配即失败，属预期。

### D-B3（已定，长期有效）：决策记录纪律

- 每个技术选择：记忆/先验 → 真源验证 → 决策 + 理由 + 代价 + 推翻条件，全部落盘（本文档 §3）。
- 推翻旧决策必须引用新证据，禁止无证据反复（U8/U9）。
- 密码/令牌只进密码文件（U6 + 记忆中的安全策略），永不进文档。

### D-B4（已定，长期有效）：mainland 机器 egress 阶梯 = 镜像源默认，代理兜底

**裁定**（用户，2026-09-11）：国内机器默认走镜像源，代理作为 fallback；直连只在
无镜像可用的源上作为末选。适用于所有未来 mainland 供给，不只本容器。

**本容器落地配方**（实测证据见 §2.4）：
1. nix channel → TUNA；cargo → rsproxy-sparse；
2. proxy 兜底走 mihomo socks5h://10.0.100.240:10809（机场，runner 同款出口），
   noProxy 放行全部有镜像/直连可用的域；
3. github archive（nix flake 抓取路径）必须走代理——直连被墙而根路径 200 的
   部分阻断是实测事实，curl 单点探测会漏判；
4. xray-191 为已验证备用（慢，~1.13MB/s）。

**决策依据**：直连快慢是单点测量（当天可用不保证明天可用），镜像源是稳定默认；
代理覆盖无镜像源；单一 curl 探测对 GFW 部分阻断会误判（本次 github 即如此）。
代价：镜像有同步滞后（TUNA channel 小时级）；rsproxy 无的 crate 仍走源站。

---

## 4. 未决问题队列（一条条来）

1. ~~devShell 工具链来源~~ → 已定（D-B2）并实证。
2. **mkShell 规范写法**：manual 示例（separateDebugInfo 一节）用 `packages` 键，
   本 flake 沿用；mkShell 现行源码路径未再深挖（低风险，manual 示例已足够权威）。
   如需深究：`nix eval nixpkgs#legacyPackages.x86_64-linux.mkShell.meta.position`
   不可用（mkShell 无 meta），可从 all-packages.nix 的 callPackage 定义链追。
3. ~~首跑 handoff 构建/测试~~ → 完成（2026-09-11）：02 号票 resolved。cargo build 退出码 0（产物 76MB）；cargo test 45 passed / 0 failed（41s 增量）。过程中抓到真问题：昨日 D6 未提交改动漏更新测试签名（codex_envelope_body 4 参→2 参），主线已修。构建内存峰值 19.35GiB（96.5% cap，未 OOM）——常规构建保持默认 -j=16，未来若增依赖可降 -j 12 留余量。
4. ~~**-j 上限**~~ → 已定（见上条）。
5. **target/ 跨机复用**：维持不复用（源码绝对路径差异 + proc-macro 缓存风险 > 收益）；迭代期若成瓶颈，用 02 号票实测时长重新权衡。
6. ~~**dragonos egress/镜像配方** → 激活，并入 `.scratch/atcd-build-infra/issues/01-provision-atcd-dev-container.md` 步骤 3。~~ → 已定并实证（2026-09-11，见 §2.4）：镜源优先（nix channel→TUNA、cargo→rsproxy），proxy 兜底 github（mihomo 主 / xray 备），cache.nixos.org 等直连。

## 5. 跟踪框架（U15）

- 本需求的 spec 与工单：`.scratch/atcd-build-infra/`（spec.md + issues/01、02）。
- matt 系列 skill 已按 `setup-matt-pocock-skills` 初始化：Local markdown tracker、
  默认五标签、AGENTS.md 承载 Agent skills 块（用户三项确认：local / 默认标签 /
  AGENTS.md）。
- 工作流：新需求先登记 .scratch，PM 打磨为 spec+工单后派 subagent；PM 只写
  spec/记录/验收，不写实现；验收开真文件核验，IRC 摘要不算证据。
