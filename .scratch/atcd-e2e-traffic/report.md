# R11 报告 — 三真实 agent 经 atcd 打 mock 上游：双端 wire 捕获与改写差异分析

Status: done（待 PM 验收）
执行: E2E-Traffic agent · 2026-09-11
Spec: `.scratch/atcd-e2e-traffic/spec.md`（Acceptance 六条逐条对照见 §8）

> 本报告数据来自最终归档轮（evidence/ 当前内容）。首轮捕获（结构性结论一致，6+2+15 对）
> 因 agent stderr 未落盘被整体重跑替换；分析管线 `tools/analyze.py` 对两轮数据均验证通过，
> 结论可复现。

---

## 1. 执行概要

| 下游 agent | 版本 | 轮次（打穿 atcd→mock 的请求对数） | agent 退出码 | wire 证据 |
|---|---|---|---|---|
| codex exec | 0.153.4 | 6 对（1 首发 + 5 重连） | 1（预期：哑 SSE 解码失败） | `evidence/codex/` |
| opencode run | 1.18.29 | 2 对（1 主请求 + 1 辅助小请求） | 1（预期：SSE 事件 zod 校验失败） | `evidence/opencode/` |
| omp -p | 18.1.16 | 13 对（持续重试，150s 由实验 timeout 截断） | 124（实验 timeout，非桥故障） | `evidence/omp/` |

全部 21 对请求 content-plane 摘要逐对匹配（`content_digest_match=True`），无 orphan、无错配。
拓扑：`agent → :19405(捕获代理) → :19404(atcd serve) → :19499(mock 上游)`，单 atcd 实例、
单 mock 账号 `acc-r11`，全程未触碰真实上游/真实账号。

## 2. 环境与原始命令记录

**atcd 二进制 provenance**：本次 E2E 全程使用容器产物
`root@10.0.100.244:/opt/atcd/target/debug/atcd`（构建于 2026-09-11 18:55 +0800），运行前
拷贝到工作站 `target/debug/atcd`。HEAD 等价性证明（三重）：
① md5 扫描 `src/atcd/{proxy,rewrite,persona,scheduler,store,oauth}.rs`、`src/bin/atcd.rs`、
`Cargo.lock`——工作站 HEAD 与容器源码逐一相同；
② 拷贝后在容器重跑 `nix develop -c cargo build --bin atcd` → "Finished in 2.20s"
（cargo 判定该产物对当前源码完全新鲜，无重编译）；
③ HEAD 末提交 0ede925（16:43 UTC）的 wall-clock 晚于产物 mtime，属"先构建验证后提交"的
时序（R10a 修复先 rsync 构建后落 commit），内容等价性以 ①② 为准。
该产物与终审绿（fmt/clippy/test 50 passed）同一构建路径。说明：工作站本地
`nix develop -c cargo build` 因 worktree 位于父 workspace 内而报 "believes it's in a
workspace"（cargo 上溯到父仓 Cargo.toml），与 `.scratch/atcd-build-infra` 记录的既定流程
一致（工作站改码 → rsync → 容器构建），故采用容器产物。

**版本**：codex-cli 0.153.4（/etc/profiles）；opencode 1.18.29（`nix run nixpkgs#opencode`，
store path `6pw7n475…-opencode-1.18.29`）；omp 18.1.16（~/.bun/bin/omp）。

**atcd serve**（RUST_LOG=debug, ATCD_MIN_TURN_GAP_MS=50, ATCD_JITTER_MS=0）：

```
ATCD_DB=…/evidence/atcd.db ATCD_UPSTREAM=http://127.0.0.1:19499 \
  ATCD_LISTEN=127.0.0.1:19404 ./target/debug/atcd serve
```

账号导入（mock 凭据，非真实；本轮全新 db）：

```
echo '{"account_id":"acc-r11","refresh_token":"rt","access_token":"at-x","expires_at":<now+86400>}' \
  | ATCD_DB=…/evidence/atcd.db ./target/debug/atcd import
# → imported acc-r11 installation=aafeb6c0-6c71-4970-897c-57863bbc8ee2 version=0.153.4
```

**三 agent 下游命令**（均指向捕获代理 :19405；`> agent_out.log 2>&1` 全量落盘）：

```
# codex（CODEX_HOME=/tmp/r11-codex-home，config.toml: base_url=http://127.0.0.1:19405/v1, wire_api=responses）
CODEX_HOME=/tmp/r11-codex-home CAPTURE_KEY=dummy-key-for-shape-capture \
  timeout 120 codex exec --skip-git-repo-check "say ok"          # exit 1

# opencode（XDG_CONFIG_HOME=/tmp/r11-oc-config，provider.capture baseURL=http://127.0.0.1:19405/v1）
timeout 180 nix run nixpkgs#opencode -- run "say ok"             # exit 1

# omp（隔离 profile ~/.omp/profiles/e2ecapture/agent/models.yml，未动 live models.yml）
timeout 150 omp --profile e2ecapture -p --no-session --model capture/gpt-5.5 "say ok"  # exit 124
```

omp 隔离 profile 说明：live `~/.omp/agent/models.yml` **未修改**（无需备份/还原）。
`omp --profile e2ecapture` 使用独立 `~/.omp/profiles/e2ecapture/agent/`（auth/sessions/models
隔离），其中写入临时 capture provider（baseUrl 指向 :19405，api=openai-responses）。
跑完保留该 profile 供复核，不影响任何 live 配置。

上游 mock：`.scratch/atcd-e2e-traffic/tools/mock_capture.py`（scripts/mock_upstream.py 的落盘
改造版：按请求序号写 `<agent>/upstream/reqNNN.upstream.{headers.json,body.bin}`，记录请求行；
SSE 响应形状与原版一致）。下游捕获代理：`tools/capture_proxy.py`（TCP 字节级转发 + 下行方向
按连接落盘 `connNNN.downstream.raw`）。分析器：`tools/analyze.py` → 每 agent `analysis.json`。

## 3. 维度一：atcd 改写清单（header 级 + body 级）

三个方言走同一单路径（D1），差异全部可枚举如下。逐对明细见
`evidence/<agent>/analysis.json`；canonical 首轮对照件
`evidence/<agent>.downstream.{headers.txt,body.json}` ↔ `evidence/<agent>.upstream.{headers.txt,body.json}`。

### 3.1 codex exec 0.153.4（自带完整身份头；conn001↔req000，body 46256B→46256B）

Header 级（7 处）：

| 动作 | header | 下游值 → 上游值 |
|---|---|---|
| 换 | authorization | `Bearer dummy-key-for-shape-capture` → `Bearer at-x`（账号 token） |
| 加 | chatgpt-account-id | — → `acc-r11` |
| 加 | x-codex-installation-id | — → `aafeb6c0-6c71-4970-897c-57863bbc8ee2`（账号人设；下游未带头，仅 turn 元数据里自带自己的 `d9fda6ce-…`） |
| 换 | x-codex-turn-metadata | JSON 内**仅** `installation_id` 一字段替换（d9fda6ce→aafeb6c0），其余 16 字段逐一相同 |
| 换 | originator | `codex_exec` → `codex_cli_rs` |
| 换 | user-agent | `codex_exec/0.153.4 (…) WindowsTerminal (codex_exec; 0.153.4)` → `codex_cli_rs/0.153.4 (…) WindowsTerminal`（账号人设钉扎） |
| 换 | host | :19405 → :19499（链路固有） |

Body 级（逐 key 比对，13 个顶层 key 全集）：**唯一变化 = `client_metadata["x-codex-turn-metadata"]`
字符串内的 `installation_id`**（header 三投影之一，与头侧 turn-metadata 同步替换）。其余 12
key——`instructions/input/tools/tool_choice/model/parallel_tool_calls/store/stream/include/
reasoning/text/prompt_cache_key`——全部逐字节保真。
（下游 6 次重连 body 全同（1 个 distinct hash），上游 6 次输出也全同——同轮重发语义被忠实
保持：turn_id / turn_started_at 不因重连刷新，与下游行为一致。）

### 3.2 opencode 1.18.29（无 codex 身份头，带 x-session-id；conn002↔req001 主请求，body 46734B→46734B）

Header 级（7 处）：

| 动作 | header | 下游值 → 上游值 |
|---|---|---|
| 换 | authorization | dummy → `Bearer at-x` |
| 加 | chatgpt-account-id | — → `acc-r11` |
| 加 | x-codex-installation-id | — → `aafeb6c0-…` |
| 加 | originator | — → `codex_cli_rs` |
| 换 | user-agent | `opencode/1.18.29 ai-sdk/…` → `codex_cli_rs/0.153.4 (…) WindowsTerminal` |
| 删 | connection | `keep-alive` → （hop-by-hop，由 atcd 重建连接消除） |
| 换 | host | 链路固有 |

Body 级：**零改写**——46734B → 46734B，全部顶层 key 逐字节保真，含 `prompt_cache_key`
（`ses_f6e743a81ffexshHxFenjuISEz` 原样保留）。辅助小请求（conn001↔req000，body 2534B，
无 instructions/tools/prompt_cache_key 的精简请求）同样 body 零改写（2534B→2534B）、
header 改写同表。

### 3.3 omp 18.1.16（无任何会话键；conn001↔req000，body 73964B→75042B，+1078B）

Header 级（12 处）——**身份树全套铸造**：

| 动作 | header | 值 |
|---|---|---|
| 换 | authorization | dummy → `Bearer at-x` |
| 加 | chatgpt-account-id | `acc-r11` |
| 加 | x-codex-installation-id | `aafeb6c0-…` |
| 加 | originator | `codex_cli_rs` |
| 换 | user-agent | `omp/18.1.16` → `codex_cli_rs/0.153.4 (…) WindowsTerminal` |
| 加 | session-id | `01a0918c-42b1-762f-a2e8-0bd5b3fce572`（铸造 session） |
| 加 | thread-id | `01a0918c-42b1-762f-a2e8-0bd6b213ef98`（铸造，与 session 同批） |
| 加 | x-client-request-id | = thread_id（同 codex 金样本形态） |
| 加 | x-codex-window-id | `01a0918c-…0bd6b213ef98:0` |
| 加 | x-codex-turn-metadata | 17 字段完整 codex 形态（uuid v7、真实发送时戳） |
| 删 | connection | hop-by-hop |
| 换 | host / content-length | 链路固有 / 随 body 变化 |

Body 级（2 处）：① `prompt_cache_key` 重写为铸造 session；② `client_metadata` **新增**
（内嵌 17 字段 turn-metadata JSON，与 header 三投影一致）。其余
`instructions/input/tools/reasoning/max_output_tokens/include/store/stream/model` 全部逐字节保真。

铸造身份的轮次推进实测：13 次重试中 `session/thread/root_turn_id/context_window_id`
稳定不变（慢变量），`turn_id` 每次重试新铸 v7（13 个 distinct）、`turn_started_at_unix_ms`
为真实发送时刻（1789148283578 → 1789148428151，覆盖 150s 实验窗）——与身份平面表
"快变量每轮新值" 一致；因 omp 重试不带轮次身份，atcd 将每次 POST 视为新轮是状态机的忠实行为。

### 3.4 内容平面保真结论（逐字节口径）

三方言、21 对请求：`instructions`、`input`（含 item 顺序与字节）、`tools`、`tool_choice`、
`model`、`reasoning`、`text`、`store`、`stream`、`include`、`parallel_tool_calls`、
`max_output_tokens` 在下游与上游之间**全部逐 key 相等**（codex 11 项 / opencode 10 项 /
omp 9 项，含"缺失即两侧同缺"，无注入、无补齐、无家族适配——符合 D6 范围收窄）。
唯一的 body 差异 = 身份平面字段（§3.1–3.3 加粗项）。金样本方言对照：三 agent 下游
top-key 集与 `scripts/fixtures/` 各自金样本一致（omp 因隔离 profile 无 harness 技能注入，
instructions 25776 字符 < 金样本 117819，属捕获环境差异非方言差异；reasoning
`{effort:high,summary:auto}` 与金样本同）。

## 4. 维度二：prompt_cache_key 三态对照

| agent | ① 裸 wire（下游发出） | ② atcd 重写值（声明） | ③ mock 所见（落盘实测） | 绑定表 session_id |
|---|---|---|---|---|
| codex | `01a0918a-62c7-7813-8078-953b5512c277`（= 其 session-id） | 不改写（下游自带完整身份，会话键=下游 session） | `01a0918a-62c7-7813-8078-953b5512c277` ✅与①同 | 同值（session_key=该值） |
| opencode | `ses_f6e743a81ffexshHxFenjuISEz` | 不改写（会话键=下游 x-session-id，session_id 采纳该值） | `ses_f6e743a81ffexshHxFenjuISEz` ✅与①同 | 同值（session_key=该值） |
| omp | `01a0918c-4041-722d-b5c4-32b2bb546aae`（omp 自产） | 重写为铸造 session `01a0918c-42b1-762f-a2e8-0bd5b3fce572` | `01a0918c-42b1-762f-a2e8-0bd5b3fce572` ✅与②同 | 同值（session_key=`pck:<①>`） |

三态闭合：③ 恒等于 ①或②（mock 磁盘即 atcd 出站字节），绑定表与 wire 互证。
"重写" 仅发生在 omp（完全无会话键 → 全铸造）一支；codex/opencode 的缓存键按
"下游自带会话身份则采纳" 语义保留。

## 5. 维度三：atcd 侧观测证据

**日志**（`evidence/atcd_serve.log`，RUST_LOG=debug，22 行）：1 行启动横幅 +
21 行 `connection error: connection closed before message completed` —— 与 21 个 agent
请求一一对应：每个客户端在读到哑 SSE 后放弃并断开，atcd 如实记录下行断开。
**未产生任何逐请求 debug 行**：atcd 当前无请求级 tracing 埋点（观测面=stderr 连接错误 +
store 表），本条按实情记录，不判为缺陷。

**绑定表**（`evidence/bindings_dump.json`，sqlite 直读；`atcd accounts` 终态快照
`evidence/accounts_cli_final.txt`）：

| session_key | thread_id | session_id | root_turn_id | context_window_id | turns | 归属 |
|---|---|---|---|---|---|---|
| `01a0918a-62c7-…12c277` | 同 key | 同 key | （空） | （空） | 6 | codex |
| `pck:01a0918c-4041-…46aae` | `…0bd6b213ef98` | `…0bd5b3fce572` | `…0bd7408b3f54` | `…0bd82759e803` | 13 | omp |
| `ses_f6e743a81ffexshHxFenjuISEz` | （空） | 同 key | （空） | （空） | 2 | opencode |

B3 粘性实证：三个会话键全部钉在 `acc-r11`，多次请求（6/13/2 turns）零迁移。
**账号面记账实证**：import 后 `5h%/7d%` 为空，跑完变 `42 / 7`（binds=3）——atcd 解析了
mock 响应头 `x-codex-primary-used-percent: 42` / `x-codex-secondary-used-percent: 7` 并回写
账号配额，`last_turn_at` 随请求刷新：响应侧链路（mock→atcd 账号状态机）同样打通。

## 6. 维度四：下行可用性（mock 哑 SSE `data: {"ok":true}\n\n` 的消费情况）

| agent | 结论（一行） |
|---|---|
| codex | 传输层收到 SSE 字节，但载荷非合法 Responses 事件流 → 判 "stream disconnected before completion: … error decoding response body"，自动重连 5/5 后放弃，exit 1（与设计文档 §10 "流断开自动重连 5 次" 实测一致；全文 `evidence/codex/agent_out.log`） |
| opencode | 传输层收到 SSE 字节，ai-sdk zod 校验拒绝 `{"ok":true}`（期望 `response.*` 事件族，错误清单完整打印），重试 1 次后 exit 1（`evidence/opencode/agent_out.log`） |
| omp | 传输层收到 SSE 字节，同样校验失败但**无限重试**（150s 内 13 连发，实验 timeout 截断 exit 124；未观测到其放弃上限；`evidence/omp/agent_out.log` 仅余 "Working..."） |

**判定**：下行链路（atcd→agent 的响应转发、SSE 字节透传、响应头配额解析）在三个方言上全部可用；
语义层失败是 mock 载荷故意非法所致（spec 预期："mock 的哑 SSE 非法，agent 端会报流错误（预期，
退出码非 0 不算失败——请求 wire 已捕获即成功）"）。三 agent 均未表现出对 atcd 的协议层不兼容。

## 7. 偏差记录（相对 spec/建议步骤）

1. **首轮重跑**：首轮捕获（6+2+15 对，结构结论与最终轮一致）因 agent stderr 未落盘
   （仅重定向了 stdout），为满足"证据落盘"整体重跑一轮并替换 evidence/；分析管线未变。
   首轮曾做过 2 发 smoke 同款 curl 链路预检，最终轮未做（链路已证），故最终绑定表仅
   3 行、全部为真实 agent 会话。
2. **omp 轮次**：omp 无放弃上限、持续重试，按 150s timeout 截断，取 conn001↔req000 为
   canonical 轮，13 对全部归档。
3. **atcd 二进制**：工作站构建被父 workspace 阻断（见 §2），采用容器 `/opt/atcd` 产物；
   产物对 HEAD 的等价性经 md5 扫描 + cargo 新鲜度判定双重证明（§2），E2E 运行的二进制与
   报告分析对象为同一文件。
4. **conn000（每相 0 字节）**：hub 端口就绪探针的空连接，非 agent 流量，分析时已剔除。
5. **fixtures 复用**：opencode 金样本另有 `_turn2` 变体未参与本轮（本轮只要求 ≥1 轮/agent）。

## 8. Acceptance 逐条对照

- [x] **三 agent × ≥1 轮全部经过 atcd 打到 mock，各有 downstream+upstream 成对捕获**
      codex 6 对 / opencode 2 对 / omp 13 对，`analysis.json` 内 21/21 对配对成功、
      content_digest 全 match；canonical 对照件 `evidence/<agent>.downstream.*` ↔
      `evidence/<agent>.upstream.*`。
- [x] **差异报告：三 agent 改写清单 + 内容平面保真结论** → §3（header 表 + body 逐 key；
      结论：内容平面 21/21 对逐字节保真，唯一 body 差异=身份平面字段）。
- [x] **prompt_cache_key 三态对照表** → §4。
- [x] **绑定表/日志观测证据落盘** → `evidence/bindings_dump.json`、
      `evidence/accounts_cli_final.txt`、`evidence/atcd_serve.log`（§5）。
- [x] **下行可用性结论（三行）** → §6。
- [x] **发现 bug 只上报不改源码** → 见 §9（未改任何 `src/`；本轮零 commit、零 push）。

## 9. 上报：疑似缺口 / 待设计裁决（不改源码）

以下均有 wire 证据，按"上报不修改"纪律列出：

1. **opencode 会话的"codex 呈现"不完整**：x-session-id 键控的会话（opencode）上游请求
   **不带** codex 身份头树（无 session-id/thread-id/x-client-request-id/x-codex-window-id/
   x-codex-turn-metadata），且下游的 `x-session-id`/`x-session-affinity`（非 codex 头）、
   `accept: */*`（codex 为 text/event-stream）原样上行；body `prompt_cache_key` 保留
   `ses_…`（非 uuid v7）。与设计 §6 "身份头全套……下游缺失时才生成" 的字面规则存在张力
   （omp 无会话键时则全套铸造，两者行为不对称）。对上游呈现而言，opencode 会话是三者中
   最不像真实 codex 的。是否属预期（"下游给了会话键即尊重之"）待设计裁决。
2. **opencode 绑定行的 thread/root/window 为空**（`bindings_dump.json`），与 codex/omp 绑定
   行的字段完整度不一致；若上游以绑定表做会话画像，此行为空字段是否可接受待裁决。
3. **omp 重试每次新铸 turn_id/turn_started_at**（13 重试 13 个 turn）：状态机对"重试 vs
   新轮"无区分信号（下游不带轮次身份时无法区分），每 POST 记一回合使 `turns=13` 与
   实际"1 轮对话" 语义偏差。计费/限速视角影响待产品裁决。
4. **atcd 无请求级日志埋点**：RUST_LOG=debug 下除连接错误外无任何逐请求行（§5），观测
   排障只能靠 store 表。非缺陷，提请台账评估（如 tracing 需求）。

## 10. 证据文件清单

```
evidence/
├── codex/                     conn001-006.downstream.raw × req000-005.upstream.{headers.json,body.bin}
│                              agent_out.log（816B，重连 5/5 全文）· analysis.json（6 对全量 diff）
├── opencode/                  conn001-002 × req000-001 · agent_out.log（15.3KB，zod 错误全文）· analysis.json
├── omp/                       conn001-013 × req000-012 · agent_out.log · analysis.json
├── codex.downstream.{headers.txt,body.json} ↔ codex.upstream.{headers.txt,body.json}   （opencode./omp. 同构）
├── atcd_serve.log             RUST_LOG=debug 全量 22 行
├── bindings_dump.json         accounts + bindings 全表
├── accounts_cli_final.txt     atcd accounts 终态（binds=3, 5h%=42, 7d%=7）
├── atcd.db (+-wal/-shm)       atcd 原始 store（可直接 sqlite 复核）
├── mock.log / proxy.log       上游与捕获代理 stderr（0B = 无错误）
tools/
├── mock_capture.py            mock 上游（落盘改造版，响应形状同 scripts/mock_upstream.py）
├── capture_proxy.py           下行 wire 字节级捕获代理
└── analyze.py                 配对 + header/body 深度 diff（可重跑复现本报告全部数字）
```

复核入口：`python3 tools/analyze.py codex opencode omp`（幂等，重算 21 对 diff）；
`sqlite3 evidence/atcd.db 'select * from bindings'`。
