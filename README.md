# agent-trace-gateway

会话级 agent 轨迹网关：部署在 agent 客户端与模型上游之间，透明转发业务流量，同时把多轮对话重组为会话级轨迹导出到 Langfuse。

## 仓库是干什么的

这是一个**独立的观测中间件（observability proxy），不是任何其他网关的功能模块，也不是 omp 的 provider**。

它从 sub2api 进程内的 modeltrace 模块（Go 约 5,913 行）抽取独立而来。抽取的动因：

1. sub2api 每次同步上游基线，trace 钩子都是冲突热点；
2. 审计逻辑与路由、鉴权、计费绞在一起，改动互相回归；
3. 旧形态锁死在"单次模型请求"，表达不了 agent 多轮轨迹（用户请求 → tool use → 响应的完整序列）。

抽取后的形态：**独立进程 + 独立控制面 + fail-open**。sub2api 感知不到它存在；它挂了，sub2api 不受影响。规格与压力检验见 sub2api 仓库 `openspec/changes/extract-agent-trace-gateway/`（proposal/specs/design/tasks，CLI 严格校验通过）。

## 架构

```
                         数据面（透明转发，不阻塞业务）
  ┌──────────┐   HTTPS   ┌───────┐  HTTP/1.1、h2c  ┌────────────────────┐  ┌──────────┐
  │ omp /    │ ────────▶ │ Caddy │ ──────────────▶ │ agent-trace-gateway │─▶│ sub2api  │──▶ 模型上游
  │ codex /  │  TLS 边缘  │       │                 │     Pingora 代理    │  │ (上游网关)│    (真实 provider)
  │ claude-cli│           └───────┘                 └─────────┬──────────┘  └──────────┘
  │ (任意客户端)│                                              │ 旁路 OTLP（控制面）
  └──────────┘                                              ▼
                                         OTel Collector ─▶ Langfuse
```

- **数据面**：Pingora 反向代理。透明转发 HTTP/1.1、HTTP/2（h2c）、WebSocket 升级。只重写 Host 头（支持 `server.sni` 覆盖），不改报文语义，不脱敏。
- **控制面**：旁路解包。按协议解包请求/响应，重组 SSE 流与 WS 帧，做会话串联，导出 OTLP。导出失败只计数、不重试、fail-open——业务流量永不因导出失败被阻断（AGENTS.md 硬约束）。

### 模块结构（`src/`）

| 模块 | 职责 |
|---|---|
| `bin/gateway.rs` | 入口。`--config <path>` 装载完整 TOML 配置文件（见「配置」章）；`key-fp --config` 复算 API key 指纹 |
| `lib.rs` | Pingora `ProxyHttp` 薄 filter：转发钩子 + `/__atg/*` 控制端点 + `logging` 阶段做解包收尾 |
| `metrics.rs` | 零依赖定桶直方图 + Prometheus 文本渲染（`/__atg/metrics`，v0.3.12） |
| `trace/unpack.rs` | 协议识别（Anthropic Messages / OpenAI Responses / OpenAI chat completions）、非流式解包、SSE 重组、tool call 提取 |
| `trace/session.rs` | 显式会话 ID 提取：body 优先于 header，按协议走不同字段路径（移植自 sub2api session.go 的 9 来源优先级） |
| `trace/prefix.rs` | 前缀指纹拼接：只存滚动 SHA256 指纹链（每轮 32 字节），不存历史原文；LRU 上限 10 万会话 / TTL 24h |
| `trace/ws.rs` | WebSocket 帧解析（升级后字节是裸流，需自行按 RFC6455 解码去掩码） |
| `trace/capture.rs` | 内容大小上限（默认 16 MiB，`capture.max_bytes`）+ 确定性截断标记 |
| `trace/store.rs` | 进程内 turn 记录存储，`/__atg/records` 快照 |
| `trace/export.rs` | 专用 tokio runtime + 有界队列（1024）+ 500ms 批量 OTLP/HTTP 导出；endpoint URL userinfo 支持 Basic Auth |

### 轨迹模型：session → turn → tool 序列

- 顶层是会话：显式 ID（header/body，三协议各有通道）或无 ID 时的 messages 前缀指纹；
- 每个 HTTP/WS 回合是一条 turn（对应 Langfuse 一条 trace）：重组后的用户输入、最终输出、tool call 列表、时延与终态；
- 会话串联三轨制：Anthropic 用 `x-claude-code-session-id` + `metadata.user_id`；Responses 用 `session-id`/`x-codex-turn-metadata` + `client_metadata.session_id`；chat completions 无显式 ID，用凭据隔离 + messages 前缀指纹；
- 内容保真：业务内容原样记录（用户明确决定不脱敏，下游清洗由数据 Owner 负责），剥离 TCP/TLS 层与逐跳传输头。

## 生产流量拓扑

```
生产 compose 栈（docs/deployments/ 形态）：
Caddy（TLS 边缘）→ agent-trace-gateway 容器 → sub2api 容器 → 模型上游
                          └─ OTLP → OTel Collector → Langfuse
```

- Caddy 终结客户端 TLS，模型路由指向网关容器；网关配置 `server.upstream` 填 sub2api 容器名:端口（compose 内网 DNS）；
- 容器 `restart: unless-stopped` 自动拉起；持续不健康时人工切 Caddy 路由回直指 sub2api 即回滚；
- **单实例部署**：前缀拼接器是进程内有状态设计，多实例会分裂拼接状态；多实例演进的前置条件是 sticky routing 或共享状态（当前列为非目标）；
- 网关只持有转发所需最小信息：不读 sub2api 数据库、不调其内部 API，鉴权仍由 sub2api 执行。

## 如何应用

### 本地运行

```bash
# 构建（release，含容器内所需的全部测试）
cargo build --release --bin gateway

# 运行：--config 提供完整配置文件（模板见下「配置」章）
./target/release/gateway --config /etc/atg/atg.toml

# bench 变体（仅功能 debug 隔离用；其数字不得进入任何性能结论）：
# 只有该编译特性下才接受 trace.mode = "off" 直通档
cargo build --release --bin gateway --features bench-trace-mode
```

接入客户端：把任意 OpenAI-compatible / Anthropic 客户端的 baseURL 指到 `http://127.0.0.1:6180` 即可，客户端零改动。

接入客户端：把任意 OpenAI-compatible / Anthropic 客户端的 baseURL 指到 `http://127.0.0.1:6180` 即可，客户端零改动。

### omp（Oh My Pi）接入

在 `~/.omp/agent/models.yml` 新增**独立测试 provider**（不得修改/复用生产 provider，也不得把现有 provider 的 baseUrl 改成网关地址借道）：

```yaml
providers:
  atg-test:
    baseUrl: http://127.0.0.1:6180
    # ... 其余字段照抄生产 provider 形态
```

验证完成后切回原 provider。任何对该文件的改动须满足"独立 provider + 可还原"。

### 生产部署

```bash
cd deploy/compose
# 先编辑 ./atg/atg.toml：upstream 指向 sub2api 容器、salt 填真实密钥、
# otlp_endpoint 按需填 Langfuse 采集端（userinfo 自动转 Basic Auth）
docker compose up -d agent-trace-gateway
```

compose 以 `gateway --config /etc/atg/atg.toml` 启动，`./atg/` 目录以只读卷挂载到 `/etc/atg/`（目录挂载：单文件 bind mount 下用 `mv` 换文件会换 inode，容器内看不到新内容）。改配置后 `docker kill -s HUP <container>` 即热重载。

### 控制端点

| 端点 | 内容 |
|---|---|
| `GET /__atg/health` | 自证 + 健康计数（JSON）：`version` / `variant`(`prod`\|`bench`) / `trace_mode`(`full`\|`off`)、导出计数与队列深度、排队面（`inflight` / `inflight_high_water` / `awaiting_upstream` / `worker_threads`）、串联表状态（`stitch_entries` / `stitch_capacity` / `stitch_expired_total` / `stitch_evicted_total`）、锁归因（`stitch_wait_ns_total` / `stitch_hold_ns_total` / `store_wait_ns_total` / `store_hold_ns_total`）、turns 计数、配置自证（`config_path` / `config_sha256` / `config_generation` / `config_loaded_at_ms` / `config_restart_required` / `config_reload_failures`） |
| `GET /__atg/config` | 只读配置视图（v0.4.0）：`config`（文件路径、sha256、generation、restart_required、reload_failures）+ `effective`（18 键生效值，`api_key.salt` 恒为 `"***"`）+ `structural`（四个结构键的**运行中**值 vs 文件值） |
| `GET /__atg/metrics` | Prometheus 文本（v0.3.12）：`atg_info` 自证标签（version/variant/trace_mode/trace_tag）、排队/背压 gauge、OTLP 导出计数与队列、四段耗时直方图 |
| `GET /__atg/records` | 进程内已收集 turn 记录 JSON 快照（调试用，内存有界） |

**观测面判读（v0.3.12）**：`atg_requests_inflight` = 网关内请求数（`new_ctx`→`logging`
括号）；`atg_requests_awaiting_upstream` = 其中尚未见到上游响应头的（网关排队 + 上游
首字节等待）；`atg_stage_time_to_first_byte_seconds` = ATG 内首字节，即记录里
`completion_start_ns − start_ns` 的聚合；`atg_stage_wait_upstream_seconds` = 请求起点到
上游响应头（含上游排队）。每请求成本为定值原子操作（无锁、无分配）；压测对照按
±5% TTFB 门验证。`atg_stitch_entries` = 前缀串联表当前链数——**它是串联锁内 O(表) 清扫
成本的规模因子**（ATG#5），部署侧用它判断该串行点在本实例负载下是否构成瓶颈。
`atg_stitch_wait_ns_total` / `atg_store_wait_ns_total`（及对应 `_hold_`）为**累计纳秒**：
除以 `turns_total` 即得每请求的锁等待/持有时长，跨线程数对比可量化"多线程代价被锁吃掉多少"。

**排障入口（v0.4.0 增补）**：配置面自证直接读 `/__atg/config`——`config_sha256`
核对"进程跑的是哪份文件"（比对 `sha256sum atg.toml`）、`config_generation` 核对
"经历了几次成功重载"、`config_reload_failures > 0` 说明有过热重载失败（旧配置
仍在服务）、`restart_required` 非空说明文件里有结构键改动在等重启；
`/__atg/health` 的 `config_*` 同名字段供告警直接消费。

### API Key 指纹（client_key_fp）

请求的 API 凭据以盐化指纹落 trace metadata（`langfuse.trace.metadata.client_key_fp`）——同 key 同指纹（跨协议），明文永不离开网关。

- **公式（可复算）**：`sha256(salt || key)` 的 UTF-8 字节流，取结果前 16 个 hex 字符
- **salt**：配置文件键 `api_key.salt`（密钥；在 `/__atg/config`、日志、记录中恒显示为 `"***"`）
- **复算**：`gateway key-fp --config /etc/atg/atg.toml <api-key>`（与运行时同一代码路径、同一 salt 来源，输出即 trace 里的 client_key_fp）
- 无凭据请求（内部探针）→ 字段缺省，不报错

### Header 证据面（hdr.*）

每条 trace 携带**除黑名单外的全部请求 header** 原值证据（issue #32）——归因争议时可直接回答"客户端到底带了哪些 header、值是什么、session id 由谁供的"。

- **黑名单（安全边界，命中即丢弃）**：
  - 凭据载体：`authorization`、`proxy-authorization`、`x-api-key`、`api-key`、`x-goog-api-key`；
  - cookie：`cookie`、`set-cookie`；
  - 逐跳传输头（README 约定剥离的传输噪声）：`connection`、`keep-alive`、
    `proxy-connection`、`te`、`trailer`、`transfer-encoding`、`upgrade`；
  - 名字含 `authorization` / `cookie` / `secret` / `password` / `credential` /
    `signature` / `token` / `bearer` / `api-key` / `apikey` / `access-key` /
    `private-key` 的 header（防客户端自造凭据头；代价是名字含 token 的限流头被误伤）。
- **属性**：`langfuse.trace.metadata.hdr.<header-name>`，仅在 header 存在且未被丢弃时写入。
- **截断**：值按**字符**截前 256（与 `client_ua` 同一函数，按 codepoint 边界，不切坏 UTF-8）。
- **边界**：单 turn 最多 32 条、名字+值总字节 ≤ 4096、名字长度 ≤ 64；超出的条目丢弃并计入
  `/__atg/health` 的 `hdr_evidence_dropped` 与 `atg_hdr_evidence_dropped_total`
  （**黑名单命中不算丢弃**，那是策略而非溢出）。
- **永不收录**：见黑名单——凭据只有 `client_key_fp` 一个 trace 侧代表。
  `/__atg/records` 与 OTLP payload 的 wire 测试以哨兵值断言明文不出现。
- **会话来源**：`langfuse.trace.metadata.session_source` =
  `body:<path>`（协议 body 源）/ `body:harness`（harness 形状规则）/
  `header:<name>` / `harness-header:<name>` / `stitcher`；无 session 时不写。

## 配置

v0.4.0 起，**TOML 配置文件是唯一的配置面**：`gateway --config <path>`。所有
`ATG_*` 环境变量退役——进程对它们**静默忽略**（不告警、不失败），迁移部署时旧
env 残留不会改变任何行为，但也不会生效。

### 文件契约

- **完整文件**：18 键缺一不可（没有隐式默认值，不存在"只写想改的键"）；
- **`version = 1` 必填**：未知版本是硬错误；
- **未知键拒绝**（每一层 `deny_unknown_fields`）——拼写错误在启动时即暴露；
- **值域校验**：`stitch.capacity` / `stitch.ttl_ms` 不再接受 `0`（v0.3.13 env
  语义收紧），`server.worker_threads` / `drain.timeout_secs` / `capture.max_bytes`
  / `store.*` / `export.max_inflight` / `export.workers` 均 ≥ 1；
- 启动时任何配置错误都是致命的，统一一行 stderr 并 exit 2：
  `agent-trace-gateway: config error: <detail> (file: <path>)`。

### 完整示例（生产 canary 18 键）

```toml
# agent-trace-gateway 配置文件（完整文件契约：18 键缺一不可，未知键拒绝）
version = 1

[server]
listen = "0.0.0.0:6180"           # 结构键
upstream = "http://sub2api:8080"  # 热键：reload 后新请求即切上游
sni = ""                          # 热键；空 = 取上游 host
worker_threads = 1                # 结构键；>1 是显式 opt-in（见 ATG#1）
max_ws_frame_payload = 0          # 热键；0 = 不限

[trace]
tag = "line:atg"                  # 热键
mode = "full"                     # 结构键；仅 bench 构建接受 "off"

[drain]
on_cancel = false                 # 热键
timeout_secs = 60                 # 热键

[capture]
max_bytes = 16777216              # 热键（16 MiB）

[store]
max_records = 1000                # 热键
max_bytes = 268435456             # 热键（256 MiB）

[stitch]
capacity = 100000                 # 热键
ttl_ms = 86400000                 # 热键（24h）

[api_key]
salt = "<SALT>"                   # 热键；密钥，任何视图/日志中恒为 "***"

[export]
otlp_endpoint = ""                # 热键；空 = 导出关闭，运行时填入即热启用
max_inflight = 8                  # 热键（内存上界 = 在途批数 × 批载荷）
workers = 4                       # 结构键（导出子系统序列化并行度）
```

（同内容随仓库发布：`deploy/compose/atg/atg.toml`。）

### 热键 vs 结构键

`kill -HUP <pid>` 触发热重载：文件被事务性重读（解析 + 全量校验通过才换
snapshot），失败则**旧配置继续服务**并计入 `reload_failures`。

| 热键（reload 即时生效） | 结构键（需重启；reload 列入 `restart_required`，运行值不变） |
|---|---|
| `server.upstream` | `server.listen` |
| `server.sni` | `server.worker_threads` |
| `server.max_ws_frame_payload` | `trace.mode` |
| `trace.tag` | `export.workers` |
| `drain.on_cancel` / `drain.timeout_secs` | |
| `capture.max_bytes` | |
| `store.max_records` / `store.max_bytes` | |
| `stitch.capacity` / `stitch.ttl_ms` | |
| `api_key.salt` | |
| `export.otlp_endpoint` / `export.max_inflight` | |

`export.workers` 之所以是结构键：导出子系统自有 runtime 的线程数在 `start`
时定型，运行中重建整个导出池意味着放弃在途批与队列状态——这笔风险不换，改完
重启即可（`config_generation` 会告诉你进程当前跑的是文件的第几代）。

`trace.mode = "off"`（capture-off 直通档）仍然是 **bench 编译特性**
（`--features bench-trace-mode`）下的能力：生产构建在配置校验时直接拒绝
`"off"`。理由不变：该档跳过的正是本仓库的用途，其数字描述的是一个永不部署的
配置，只能用于功能 debug 隔离。

### 重载操作面

```bash
# 部署前预检（不启动服务）：解析 + 全量校验，打印指纹与生效值（salt 脱敏）
./gateway config check /etc/atg/atg.toml

# 改完文件后热重载（热键即时生效；结构键列入 restart_required，不中断服务）
kill -HUP $(pidof gateway)

# 只读核对：文件指纹、generation、restart_required、生效值（salt 脱敏）
curl -s http://127.0.0.1:6180/__atg/config | jq .
```

`GET /__atg/config` 返回三块：`config`（path / sha256 / generation /
loaded_at_ms / restart_required / reload_failures）、`effective`（18 键生效
值，`api_key.salt == "***"`）、`structural`（四个结构键的**运行中**值——与文
件不一致即说明该键在等一次重启）。`/__atg/health` 同步携带
`config_path` / `config_sha256` / `config_generation` /
`config_restart_required` / `config_reload_failures`，供告警与巡检直接读。

### 从 env 迁移

v0.3.13 部署升级到 v0.4.0：把 compose/systemd 里的 `ATG_*` 环境变量按上表换算
成一个完整 `atg.toml`（启动命令改 `--config`），删除全部 `ATG_*` env。旧 env
即使残留也不会生效——升级的第一步永远是核对 `GET /__atg/config` 的
`effective` 与文件一致。

## 设计边界（刻意不做）

- 不做多后端路由/账号池/failover（对 sub2api 透明转发）；
- 不捕获 sub2api 内部事实（attempt、账号、usage/cost、异步 batch 执行；usage 经 Langfuse 外对账）；
- 不做多实例 HA（有状态拼接限制，演进前置条件见上）；
- 不做轨迹存储/查询 UI（消费端仍是 Langfuse）；
- 不承担下游数据清洗（原样内容的脱敏与治理由数据 Owner 在 Langfuse 侧负责）。

## 版本与发布

- 遵循[语义化版本](https://semver.org/lang/zh-CN/)（`X.Y.Z`）；变更明细见 [CHANGELOG.md](CHANGELOG.md)（Keep a Changelog 格式）。
- **tag 即 release**：push `vX.Y.Z` annotated tag 触发 CI，构建镜像并自动创建 GitHub Release（附镜像 digest）。历史锚点：`v0.1.0` = `df0d32a`。
- 镜像 tag 对应关系：`X.Y.Z` 与 `X.Y` 由 release tag 生成并固定；`:latest` 与 `:<sha>` 随 main 每次构建覆盖。
- **部署必须 pin digest**（Release 页可查），勿用 `:latest`/`:sha`——两者会被后续 CI 覆盖。

## 测试与回归

- `tests/`：行为测试（TDD 主战场）——协议解包、SSE/WS 重组、会话串联、内容保真、体积上限、fail-open、OTLP 导出；
- bench 变体测试（`tests/trace_mode.rs`）只在 `cargo test --features bench-trace-mode` 下编译运行；默认档的"遗留 env 被忽略"不变量由 `tests/legacy_env_ignored.rs` 守（v0.4.0 起 env 是退役配置面，测试全量走完整配置文件 fixture）；
- `xtask/harness/`：回归 harness（协议 fixture 上游 + 测试驱动客户端）；
- `xtask/harness/fixtures/`：真实抓包样本（去凭据，版本库卫生要求）；运行时轨迹按 D4 原样记录，两者互不影响。

## 开工决定记录

- 仓库归属：**Vitus213**（用户决定，不进 Alle-Group）；
- 规格与压力检验：见 sub2api 仓库 `openspec/changes/extract-agent-trace-gateway/`（proposal/specs/design/tasks，CLI 严格校验通过）；
- 开工决定：远端仓库由 Vitus213 名下创建（tasks 0.2 授权门禁已满足）；
- 迁移阶段：1 双跑（网关上线，sub2api modeltrace 保留，Langfuse 按会话对照新旧轨迹覆盖率）→ 2 切换（关闭 sub2api OTLP 导出）→ 3 删除（移除 modeltrace 全部钩子）；任一阶段异常，Caddy 路由切回直指 sub2api 即回滚。

## AGENTS.md

仓库规约文件 `AGENTS.md` 定义了硬性边界：omp provider 纪律、Langfuse 纪律（本地部署为主、禁连生产、禁直连 ClickHouse）、fail-open 原则。进入本仓库工作前必须阅读。