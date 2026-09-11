# 01 — 在 dragonos-1288v3 上创建 atcd-dev Incus 容器（NixOS）

Status: resolved
Blocked by: —

## 目标

在 dragonos-1288v3（incus 宿主）上新建一个专用 NixOS 容器，名为 `atcd-dev`，
作为 atcd 的构建/测试执行器。只做供给，不做构建（构建是 02 号票）。

## 环境与前提（指针，先读）

- 事实与决策台账：`docs/atcd-build-infra.md` §2（事实）§3（决策）——主机拓扑、
  资源数据、禁区都在里面。
- ssh：`ssh dragonos-1288v3`（`~/.ssh/config` 已配：100.64.0.17:2222，user dragonos）。
- sudo：dragonos 账号无免密 sudo。密码文件
  `/home/nixos/workspace/scitrace/dragonos-1288v3-sudo-key.secrets`，
  只允许 `sudo -S` 从 stdin 消费；**禁止**把密码写进命令行参数、日志、文档、
  shell 历史。
- NixOS-on-Incus 声明式配方先例：`/home/nixos/workspace/scitrace/sub2api-infra`
  （三台 runner 容器的配置）。复用其模式，不复制其凭据。
- **禁区**：sub2api-incus-1/2/3（10.0.100.241-243）与其上的 GitHub runner；
  宿主上其他容器一律不碰。

## 步骤建议（可按实际情况调整，偏差要记录）

1. `incus launch images:nixos/26.11 atcd-dev`（或 sub2api-infra 同款镜像路径），
   配置：`limits.cpu=16`、`limits.memory=20GiB`、`limits.memory.swap=false`、
   root 盘 100GiB、`security.nesting=true`（nix 需要的用户态多租户特性，runner
   先例同款）。
2. NixOS 声明配置进容器：启用 flakes（`nix.settings.experimental-features =
   [ "nix-command" "flakes" ]`）、openssh、git、rsync；注入工作站公钥
   （`~/.ssh/id_ed25519.pub`）供 samuka→容器直连；固定 IPv4（从 10.0.100.0/24
   段选一个未占用地址，避开已用清单：见台账 §2.2）。
3. egress：按 spec §2 验证 github / static.rust-lang.org / cache.nixos.org /
   crates.io 可达性；不通则配置代理（先验证 socks5 10.0.100.191:10808），
   配方落盘台账。
4. 冒烟：`nix develop -c rustc --version` 能在一个最小 flake 上跑通
   （工具链下载走通网络）。

## Acceptance（对应 spec A1/A2/A5/A6）

- [x] `incus list` 显示 atcd-dev RUNNING，`incus config show atcd-dev` 的限额与设计一致（输出全文留档到本票 Comments）
- [x] 从工作站 `ssh <atcd-dev-ip>` 直连成功
- [x] 容器内 `nix develop -c rustc --version` = 1.95.0（fenix 工具链拉取走通）
- [x] `incus list` 前后对照：sub2api-incus-1/2/3 状态与地址不变
- [x] egress 实测结果与最终配方追加到 `docs/atcd-build-infra.md`

## 报告要求

错误发生时上报：原始命令、所处阶段、资源状态、对验收项的影响、根因、
备选路径、持久化输出路径。IRC 摘要不算证据，PM 会开真文件核验。

## Comments

### 2026-09-11 执行完成（OpsAtcdDev）——逐条验收证据

证据目录：`.scratch/atcd-build-infra/evidence/01/`
（configuration.nix、atcd-dev-config-show.txt、atcd-dev-devices-state.txt、
incus-list-before.csv / incus-list-after.csv、egress-probe.sh、devshell-env.txt）。
台账事实区已追加 §2.4；未决项 §4-6（egress/镜像配方）标记已解决。

**A1. RUNNING + 限额一致 → 满足。** `incus list atcd-dev`：
`atcd-dev | RUNNING | 10.0.100.244 (eth0) | CONTAINER`。`incus config show atcd-dev`
全文如下（即 evidence/01/atcd-dev-config-show.txt）：

```
architecture: x86_64
config:
  boot.autostart: "true"
  image.architecture: amd64
  image.description: Nixos 26.05 amd64 (20260911_01:03)
  image.os: Nixos
  image.release: "26.05"
  image.requirements.secureboot: "false"
  image.serial: "20260911_01:03"
  image.type: squashfs
  image.variant: default
  limits.cpu: "16"
  limits.memory: 20GiB
  limits.memory.enforce: hard
  limits.memory.swap: "false"
  limits.processes: "8192"
  security.nesting: "true"
  volatile.base_image: 742a9eb71576fb93e7847ee43fd9105b625eb38bf21933e0a53c907e0fc6eccc
  volatile.cloud-init.instance-id: a2660e53-d523-4ae6-90a8-713a4f5fd742
  volatile.eth0.host_name: veth1150045b
  volatile.eth0.hwaddr: 10:66:6a:05:24:cf
  volatile.eth0.name: eth0
  volatile.idmap.base: "0"
  volatile.idmap.current: '[{"Isuid":true,"Isgid":false,"Hostid":1000000,"Nsid":0,"Maprange":1000000000},{"Isuid":false,"Isgid":true,"Hostid":1000000,"Nsid":0,"Maprange":1000000000}]'
  volatile.idmap.next: '[{"Isuid":true,"Isgid":false,"Hostid":1000000,"Nsid":0,"Maprange":1000000000},{"Isuid":false,"Isgid":true,"Hostid":1000000,"Nsid":0,"Maprange":1000000000}]'
  volatile.last_state.idmap: '[]'
  volatile.last_state.power: RUNNING
  volatile.uuid: 88b698f7-d024-4ade-9704-a3636346bcad
  volatile.uuid.generation: 88b698f7-d024-4ade-9704-a3636346bcad
devices:
  eth0:
    ipv4.address: 10.0.100.244
    network: incus_bridge_0
    type: nic
  root:
    path: /
    pool: default
    size: 100GiB
    type: disk
ephemeral: false
profiles:
- default
stateful: false
description: ""
```

限额运行时生效（`incus query /1.0/instances/atcd-dev/state`，留档
atcd-dev-devices-state.txt）：memory total=21474836480（=20GiB hard）、
swap_usage=0；cpu allocated_time=16000000000（=16 核）；disk root
total=107374182400（=100GiB）。

**A2. 工作站 ssh 直连 → 满足。** `ssh -o StrictHostKeyChecking=accept-new
root@10.0.100.244` 免密成功（工作站 tailscale0 有 10.0.100.0/24 table-52 子网路由，
实测 `ip route get 10.0.100.240` 即可确认）。容器内 root+samuka 均注入工作站
ed25519 公钥；PasswordAuthentication=false；samuka 免密 sudo。

**A3. `nix develop -c rustc --version` = 1.95.0 → 满足。** 工作站
`.worktrees/atcd` 的 flake.nix+flake.lock rsync 到容器 `/root/atcd-devshell` 后：

```
rustc 1.95.0 (59807616e 2026-04-14)
cargo 1.95.0 (f2d3ce0bd 2026-03-21)
pkg-config --modversion openssl → 3.6.3
LIBCLANG_PATH=/nix/store/yc2a9854a2y2c8kci88piblc847iq1l4-clang-21.1.8-lib/lib
BINDGEN_EXTRA_CLANG_ARGS=（bindgenHook 注入，全文见 evidence/01/devshell-env.txt）
```

与工作站 D-B2 实证逐项一致。冷启动全链 14.6 分钟（flake inputs 走代理、
fenix 1.95.0 组件、cache.nixos.org 闭包）；热复用秒回；切换最终代理配置后、
容器重启后各复验一次均通过。

**A4. runner 前后对照 → 满足。** `incus list --format csv` 前后快照
（evidence/01/incus-list-before.csv / -after.csv）diff 结果：**仅新增一行**
`atcd-dev,RUNNING,10.0.100.244 (eth0),,CONTAINER,0`，其余零变化；
sub2api-incus-1/2/3 前后均为 RUNNING、IP 10.0.100.241/242/243 不变。

**A5. egress 实测结论与配方 → 满足，已写入台账 §2.4。** 摘要：

| 域名 | 实测 |
|------|------|
| github.com 根 | 200 (0.87s) 直连可达 |
| github.com/*/archive/*.tar.gz | **直连不通**（45s 0 字节）——nix flake 抓取路径 |
| codeload.github.com | 200，1.1–3.9 MB/s 直连 |
| cache.nixos.org | 200，narinfo 0.67s 直连 |
| channels.nixos.org | 200（channel 拉取 109s） |
| static.rust-lang.org | 200（fenix manifest 4.6s） |
| index.crates.io / static.crates.io | 200 / 下载 206 但 ~78KB/s |
| mihomo 10.0.100.240（主出口） | socks5h:10809 → 200@0.83s、53MB 实测 3.18 MB/s；http:10808 → 200@0.84s、2.12 MB/s |
| xray 10.0.100.191:10808（备） | 200（github 2.6s / archive 3.3s），可用但慢 |

最终配方（镜源优先，proxy 只兜底）：nix channel → TUNA
`mirrors.tuna.tsinghua.edu.cn/nix-channels/nixos-26.05`（已切并 update 成功）；
cargo → rsproxy（rsproxy-sparse，activationScripts 声明式下发 root+samuka）；
`networking.proxy.default = "socks5h://10.0.100.240:10809"`，noProxy =
`127.0.0.1,localhost,10.0.100.0/24,cache.nixos.org,channels.nixos.org,static.rust-lang.org,index.crates.io,static.crates.io,mirrors.tuna.tsinghua.edu.cn,rsproxy.cn`。
github.com 与 codeload.github.com 均走代理（前者直连被墙；后者与同族防抖动）。
socks5h = 代理端解析 DNS，避 GFW 污染。mihomo 白名单双重门已含 10.0.100.244/32
（宿主 mihomo.nix 声明 + config.yaml lan-allowed-ips，mihomo-fetch.service 已跑通）。

**与工单步骤建议的偏差（均已记录）**：
1. 镜像用 `images:nixos/26.05`（runner 同款）而非建议的 26.11——26.11 镜像尚不存在
   （NixOS .05/.11 发布节奏），且复用 runner 已验证路径。
2. `incus launch --device eth0,type=nic,...` 在本机 incus 6.0.6 解析 bug（type 吞并
   逗号后缀，Failed creating instance record）→ 改 `incus create` +
   `incus config device add` 分步执行；容器最终配置与设计一致。
3. 挂 default profile（nixos-builder 同款先例），实例级 limits 覆盖 profile 值；
   runner 用空 profiles，此处无功能差异。
4. nix channel 切 TUNA、cargo 配 rsproxy、代理主出口用 mihomo（替代先验的 xray-191）
   ——按 Main 转达的用户镜源优先裁定执行，xray-191 保留为已验证备用。
5. 新增未在工单内的镜像源配置（TUNA/rsproxy）均落在 configuration.nix /
   channel 状态里，无源码仓库改动；未 git commit 任何东西。

### PM 验收（2026-09-11）

独立复测通过（非 IRC 摘要）：incus-list diff 仅 atcd-dev 一行新增；容器内 `nix develop -c` 实测 rustc 1.95.0 / openssl 3.6.3 (pkg-config) / LIBCLANG_PATH 就位；mihomo 240 socks github 200、crates 直连 200。验收通过，工单关闭。
