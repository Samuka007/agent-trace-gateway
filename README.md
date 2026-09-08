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

- **数据面**：Pingora 反向代理。透明转发 HTTP/1.1、HTTP/2（h2c）、WebSocket 升级。只重写 Host 头（支持 `ATG_SNI` 覆盖），不改报文语义，不脱敏。
- **控制面**：旁路解包。按协议解包请求/响应，重组 SSE 流与 WS 帧，做会话串联，导出 OTLP。导出失败只计数、不重试、fail-open——业务流量永不因导出失败被阻断（AGENTS.md 硬约束）。

### 模块结构（`src/`）

| 模块 | 职责 |
|---|---|
| `bin/gateway.rs` | 入口。环境变量配置（见下） |
| `lib.rs` | Pingora `ProxyHttp` 薄 filter：转发钩子 + `/__atg/*` 控制端点 + `logging` 阶段做解包收尾 |
| `trace/unpack.rs` | 协议识别（Anthropic Messages / OpenAI Responses / OpenAI chat completions）、非流式解包、SSE 重组、tool call 提取 |
| `trace/session.rs` | 显式会话 ID 提取：body 优先于 header，按协议走不同字段路径（移植自 sub2api session.go 的 9 来源优先级） |
| `trace/prefix.rs` | 前缀指纹拼接：只存滚动 SHA256 指纹链（每轮 32 字节），不存历史原文；LRU 上限 10 万会话 / TTL 24h |
| `trace/ws.rs` | WebSocket 帧解析（升级后字节是裸流，需自行按 RFC6455 解码去掩码） |
| `trace/capture.rs` | 内容大小上限（默认 16 MiB，`ATG_CAPTURE_MAX_BYTES`）+ 确定性截断标记 |
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

- Caddy 终结客户端 TLS，模型路由指向网关容器；网关 `ATG_UPSTREAM` 填 sub2api 容器名:端口（compose 内网 DNS）；
- 容器 `restart: unless-stopped` 自动拉起；持续不健康时人工切 Caddy 路由回直指 sub2api 即回滚；
- **单实例部署**：前缀拼接器是进程内有状态设计，多实例会分裂拼接状态；多实例演进的前置条件是 sticky routing 或共享状态（当前列为非目标）；
- 网关只持有转发所需最小信息：不读 sub2api 数据库、不调其内部 API，鉴权仍由 sub2api 执行。

## 如何应用

### 本地运行

```bash
# 构建（release，含容器内所需的全部测试）
cargo build --release --bin gateway

# 运行：监听 6180，转发到 sub2api
ATG_UPSTREAM=127.0.0.1:8080 ./target/release/gateway

# 可选：OTLP 导出到本地 Langfuse（URL userinfo 自动转为 Basic Auth）
ATG_OTLP_ENDPOINT='http://pk:sk@127.0.0.1:13000/api/public/otel/v1/traces' \
./target/release/gateway
```

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
ATG_UPSTREAM=sub2api:8080 ATG_OTLP_ENDPOINT='http://pk:sk@langfuse:13000/api/public/otel/v1/traces' \
  docker compose up -d agent-trace-gateway
```

### 控制端点

| 端点 | 内容 |
|---|---|
| `GET /__atg/health` | `{exported, failed, dropped}` 导出健康计数 |
| `GET /__atg/records` | 进程内已收集 turn 记录 JSON 快照（调试用，内存有界） |

### 配置项

| 环境变量 | 默认 | 说明 |
|---|---|---|
| `ATG_LISTEN` | `127.0.0.1:6180` | 监听地址 |
| `ATG_UPSTREAM` | （必填） | 上游 `host:port` / `http(s)://host:port` |
| `ATG_OTLP_ENDPOINT` | 无（不导出） | OTLP/HTTP 导出端点；URL userinfo 自动转 Basic Auth |
| `ATG_SNI` | 上游 host | HTTP/HTTPS 上游 SNI / Host 头覆盖 |
| `ATG_CAPTURE_MAX_BYTES` | 16 MiB | 单条轨迹内容捕获上限 |
| `ATG_STITCH_CAPACITY` | 100_000 | 前缀拼接 LRU 容量 |
| `ATG_STITCH_TTL_MS` | 24h | 前缀指纹 TTL |

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
- `xtask/harness/`：回归 harness（协议 fixture 上游 + 测试驱动客户端）；
- `xtask/harness/fixtures/`：真实抓包样本（去凭据，版本库卫生要求）；运行时轨迹按 D4 原样记录，两者互不影响。

## 开工决定记录

- 仓库归属：**Vitus213**（用户决定，不进 Alle-Group）；
- 规格与压力检验：见 sub2api 仓库 `openspec/changes/extract-agent-trace-gateway/`（proposal/specs/design/tasks，CLI 严格校验通过）；
- 开工决定：远端仓库由 Vitus213 名下创建（tasks 0.2 授权门禁已满足）；
- 迁移阶段：1 双跑（网关上线，sub2api modeltrace 保留，Langfuse 按会话对照新旧轨迹覆盖率）→ 2 切换（关闭 sub2api OTLP 导出）→ 3 删除（移除 modeltrace 全部钩子）；任一阶段异常，Caddy 路由切回直指 sub2api 即回滚。

## AGENTS.md

仓库规约文件 `AGENTS.md` 定义了硬性边界：omp provider 纪律、Langfuse 纪律（本地部署为主、禁连生产、禁直连 ClickHouse）、fail-open 原则。进入本仓库工作前必须阅读。