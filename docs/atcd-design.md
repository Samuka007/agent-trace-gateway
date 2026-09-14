# atcd 设计文档

> 本文件是 atcd 的唯一设计事实来源。设计结论先改这里，再改代码。
> 需求与状态跟踪见 `docs/atcd-requirements.md`。

## 1. 定位

atcd 是一个**多账号 codex 客户端池**：

- 对上游（chatgpt.com/backend-api/codex），它呈现为若干个彼此独立的真实
  codex 用户——每个账号一个人设、一个出口、一套会话状态；
- 对下游（newapi 的自定义 provider 通道），它暴露 OpenAI Responses 兼容 API。

它**不是代理**。代理改写字节；客户端生成请求。下游是谁（codex CLI、
opencode、omp、任何 Responses 方言 agent）不影响处理路径。

## 2. 两条正确性原则

| 种类 | 覆盖内容 | 保证方式 |
|---|---|---|
| **字段正确性**：字段名、类型、序列化形状、字段集完整性 | 由**复用 codex 源码**保证——请求类型、元数据结构、序列化器来自 fork（Samuka007/codex 分支 `atcd-libs`，补丁仅翻转可见性，不改语义） |
| **语义正确性**：字段值从哪里来、何时变、何时不变 | 由**本文档的映射规则表**定义；实现与表不一致即为 bug；测试验证表 |

> 历史教训（见 §8 已证伪表）：手抄 wire 形状的转写件从写完就开始腐烂
> （UA 前缀、元数据 7→17 字段、parallel_tool_calls 方向全部抄错）。
> 凡 codex 公开可调用的代码一律调用；不公开的通过 fork 补丁公开；
> 都不可行的才转写，且必须标注 codex 源码行号并纳入金样本差分。

## 3. 统一客户端模型（核心决策 D1）

所有下游请求统一三步处理，**没有家族分支、没有透传路径**：

```
解析（任意 Responses 方言 → 语义结构）
  → 映射（身份状态机推进 + 缺省填充）
    → 重建（codex 请求类型 + codex 序列化器 → 出站请求）
```

下游 codex CLI 与 opencode 的唯一差别在解析步：前者请求里已带完整身份头
（会被映射覆盖），后者不带（由映射铸造）。处理路径是同一条。

**决策记录**：
- D1：统一客户端模型，取代早期"codex 透传 / 第三方铸造"双路径设计。
- D1a：字节透传不变量（旧 B1）被取代——body 不再保真"下游字节"，
  而是"语义保真 + 结构原生"（codex 序列化器产物）。理由：
  1. 同账号请求必须呈现为同一人设版本的原生输出，与下游实际运行的
     版本/客户端无关——重建天然规范化；
  2. 下游请求中 codex 类型之外的未知字段在重建时丢弃——codex 本来
     就不会发那些字段，丢弃提升家族一致性；
  3. 删除外科替换、家族分支、header/body 身份分裂三块复杂度；
  4. 未来 WS v2 走同一套类型，翻译层不会重现。

## 4. 变与不变量表（身份平面）

| 类别 | 字段 | 生命周期 | 轮换触发 |
|---|---|---|---|
| 不变量 | installation_id | 账号一生 | 永不（导入时铸造一次） |
| 不变量 | originator / UA（版本钉扎） | 账号人设 | 人工升级（错峰） |
| 不变量 | 出口代理绑定 | 账号 | 人工调整 |
| 慢变量 | session_id / thread_id（铸造时同值） | 对话期 | 对话作废 |
| 慢变量 | prompt_cache_key（= session_id） | 对话期 | 随 session |
| 慢变量 | root_turn_id（首轮 turn_id） | 对话期 | 对话作废 |
| 慢变量 | context_window_id | 窗口期 | 压缩事件 |
| 慢变量 | window_number / window_id（thread:N） | 窗口期 | 压缩事件（N+1） |
| 快变量 | turn_id | 每轮 | 每轮新 v7 |
| 快变量 | turn_started_at_unix_ms | 每轮 | 每轮真实发送时刻 |

语义规则（由 atcd 建立，codex 代码只保证形状）：
- root_turn_id 锚定对话首轮，此后不变——直到对话作废；
- context_window_id 在窗口期稳定——压缩事件才换新；
- turn_started_at 必须是真实发送时刻（上游看得到到达时间，编造即暴露）；
- prompt_cache_key 恒等于 session_id（真实 codex 由 session 派生，
  二者相等进行过实测验证）。

## 5. 内容平面（范围外，纯透传）

**语义映射的范围 = 认证头 + 源信息（身份树）。** 以下内容不属于映射
范围，一律原样透传，不做注入、不做补齐、不做家族适配：

| 字段 | 规则 |
|---|---|
| model | 透传 |
| instructions（系统提示词） | 透传；缺失也不注入——这是下游客户端的配置责任 |
| input items | 全部历史：message / function_call / function_call_output / reasoning（含加密推理块）原样保留 |
| tools / tool_choice | 透传 |
| reasoning / text / store / include / parallel_tool_calls / stream | 请求参数，透传；缺失时按上游默认行为，后果由下游客户端配置承担（已知行为：store 缺省时上游默认存储会话） |

解析时类型之外的未知字段被丢弃——codex 本来就不会发它们。

## 6. 源信息生成规则（映射范围之内，下游缺失时才生成）

| 字段 | 生成规则 | 依据 |
|---|---|---|
| 身份头全套（session/thread/window/turn/…） | 绑定铸造身份 + codex 结构体投影 | 身份平面表 |
| prompt_cache_key | = session_id | 真实不变量（金样本实测二者相等） |
| client_metadata | CodexResponsesMetadata 投影 | codex 结构体生成 |
| x-codex-installation-id（三投影：头/turn元数据/body） | 账号人设 installation | 身份平面表 |

**注意**：范围仅限源信息。系统提示词、工具调用、请求参数一律不生成、
不补齐（决策 D6）。

## 7. 单路径映射表

```
出站请求 = codex 序列化(
    语义结构（解析自下游，内容平面原样保留）
  ⊕ 身份/源信息字段（身份平面：按表推进，缺失才生成）
)
出站头 = 身份头（身份平面） + 凭据头（账号 token / account id）
       + 客户端工件头（透传：x-codex-beta-features 等）
```

路由：每个请求按会话键（session-id → x-session-id → body
prompt_cache_key → body 前缀散列）查绑定表得账号；表无则放置并铸
造身份；放置后粘住，永不迁移。

## 8. 决策记录

| 编号 | 决策 | 取代/影响 |
|---|---|---|
| D1 | 统一客户端模型：单路径，解析→映射→重建 | 取代双路径透传设计；取代 B1 字节透传不变量；关闭 R3a |
| D2 | 字段正确性=复用源码（fork 补丁），语义正确性=映射规则（本文档） | 定义两类 bug 的归属 |
| D3 | 保真分级：调用级（复用，构造保证）> 转写级（捕获 diff 保证）> 假设（禁止） | 转写件必须标注 codex 源码行号并纳入差分 |
| D4 | 依赖以 codex 工作区为准；fork 补丁系列自动化重放到上游新版本 | 本地构建需 openssl（codex-http-client 硬依赖 native-tls） |
| D5 | 需求台账 + 映射文档为唯一事实来源；新需求入账排队，不立即切换方向 | docs/atcd-requirements.md + 本文件 |
| D6 | 语义映射范围 = 认证头 + 源信息（身份树）。系统提示词与工具调用不属于映射范围：不注入人设提示词、不补齐请求参数（store/include 等按下游原样透传，缺失的后果由下游配置承担） | 用户指令（范围收窄） |
| D7 | WebSocket V2 透传桥：wire 透传优先（消息层字节保真），身份面与 HTTP 同一套规则；桥零协议语义（不解析帧内容） | 详见 §12；关闭台账 R4 |

## 9. 验证策略：金样本差分

1. 用 nix 提供的各版本 agent（codex 0.153.4 / opencode 1.18.29 / …）
   以 custom provider 模式向捕获器发真实请求，存为金样本
   （scripts/fixtures/）；
2. 金样本回放穿过 atcd，diff 重建产物 vs 原始请求；
3. **diff 集必须恰好等于声明的映射变量集**（installation、缓存键等）——
   多一个字段、少一个字段、值不可解释，均为 bug；
4. 每次 codex 升级重跑，作为守门测试。

## 10. 实证附录：codex 0.153.4 金样本事实

捕获环境：nix codex 0.153.4，custom provider (wire_api=responses)，
fixtures: `scripts/fixtures/codex_exec_0.153.4.*`

- 身份头全套存在：session-id / thread-id / x-codex-window-id /
  x-client-request-id / x-codex-installation-id / x-codex-turn-metadata /
  x-codex-beta-features(remote_compaction_v2)
- session_id == thread_id（exec 模式）；turn 元数据 17 字段
- UA：`{originator}/{ver} ({os_type} {os_version}; {arch}) {terminal}`
  （originator 随表面变化：exec=codex_exec；os 段 os_info 渲染，
  Ubuntu 22.04 → "22.4.0" 是库行为非笔误）
- body：store:false、include:[reasoning.encrypted_content]、
  parallel_tool_calls:true、reasoning:{effort:"medium"}、
  text:{verbosity:"low"}、prompt_cache_key == session_id、
  client_metadata 7 键、instructions 21173 字符、tools 10 个
  （exec_command/write_stdin/apply_patch/view_image/get_goal/…）
- 流断开自动重连 5 次

## 11. 待办

1. fork 补丁 #2：Responses 请求类型（request struct + input items）pub 化
2. 单路径重构：删除 surgical/envelope 双分支，统一走"解析→映射→重建"
3. 金样本差分测试（§9）自动化
4. ~~R4 WebSocket V2~~ 已完成（§12 D7 桥落地：src/atcd/ws.rs；真实上游
   chatgpt.com realtime 端到端验证待有可用账号后补）
5. 金样本扩容：codex TUI、opencode 多轮、omp

## 12. D7：WebSocket V2 透传桥（realtime WS）

> 协议真源：codex fork checkout
> `codex-rs/codex-api/src/endpoint/realtime_websocket/`（下述行号均指该 checkout，
> `core/src/realtime_conversation.rs` 除外）。

### 12.1 协议事实（全部有源码依据）

1. **三种 wire adapter**：`RealtimeEventParser::{V1, RealtimeV2, FramelessBidi}`
   （protocol.rs:15-19）。v3 = FramelessBidi（`/v1/live` + call_id 路径段），
   **不在 R4 范围**；v1 与 v2 共用 `/v1/realtime` 路径族且都是 JSON 文本帧——
   桥不解析帧语义，故对 v1 天然兼容。R4 目标 = RealtimeV2。
2. **URL 推导**（methods.rs:1077-1202）：provider.base_url → scheme
   http→ws / https→wss；路径归一（`normalize_realtime_path`，非 frameless 分支）：
   空/`/` → `/v1/realtime`；`…/realtime` 保持；`…/realtime/` 去尾斜杠；
   `…/v1` 追加 `/realtime`；`…/v1/` 追加 `realtime`；**其他路径原样保持**。
   query：intent（v1=`quicksilver`，v2=无，methods_v2.rs:178-180）+ model
   （可选）+ provider.query_params；call_id 对 V1/V2 走 query
   （methods.rs:1159-1161）。
3. **帧格式**：出站消息全部是 serde 序列化的 JSON **单 Text 帧**
   （protocol.rs:50-85 `RealtimeOutboundMessage` 全集：input_audio_buffer.append /
   input_audio.append / conversation.item.create / session.update /
   response.create / session.close / conversation.handoff.append /
   delegation.context.append / session.context.append）；入站按 `"type"`
   字段分发（protocol_v2.rs:24-79，v2 事件全集见该 match）。
   **无 Binary 帧**（v2 客户端收到 binary 报错，methods.rs:570-574）、
   **无子协议**（握手全源码无 Sec-WebSocket-Protocol）、
   **无 permessage-deflate**（realtime 用 `WebSocketConfig::default()`，
   methods.rs:1073-1075；对比 responses WS 显式启用压缩
   responses_websocket.rs:559-566）。
4. **帧内无身份投影点**：出/入站 v2 类型全集（protocol.rs:50-85、
   protocol_v2.rs:27-77）与 session.update 结构（methods_v2.rs:75-169）
   均无 installation / client_metadata 字段。**installation 在 WS 通道的
   帧投影 = 空集**。
5. **握手头**（真实 codex v2 出站，realtime_conversation.rs:1799-1833 +
   headers.rs:5-14 + default_client.rs:335-351）：
   `authorization: Bearer <api_key>`（realtime v2 现仅支持 API key 认证，
   realtime_conversation.rs:1773-1797）、`x-session-id`（realtime 会话 id）、
   `originator`、`session-id` / `thread-id`、`x-codex-turn-metadata`（可选）、
   `user-agent`（default_headers 兜底槽）。**不含**
   `x-codex-installation-id` / `chatgpt-account-id`。
6. **心跳/超时/关闭**：无应用层 ping（客户端不主动 ping）；收 Ping 即回
   Pong（methods.rs:123-130，由 tungstenite 读路径自动完成）；无读空闲超时
   （realtime 事件循环无限阻塞）。Close 帧对 v2/v1 = 会话流正常结束
   （methods.rs:548-557，`Ok(None)`）；CloseCode 区分语义仅 v3 使用
   （methods.rs:558-568）。

### 12.2 设计

**拓扑**：codex 客户端 ──ws──▶ atcd ──wss(+socks 出口)──▶ ATCD_UPSTREAM。
两条 tungstenite `WebSocketStream`，单任务 `select` 双向转发。

**D7.1 保真目标（B1 在 WS 通道的落点）**：**WebSocket 消息层字节保真**——
Text/Binary 帧 payload 原样转发（不解析、不重序列化、不重编码）；Close 帧
code/reason 原样转发。Ping/Pong 是连接级控制帧：由本侧 tungstenite 读路径
自动应答（RFC 6455 要求 Pong 走同连接；跨连接转发属协议违规），**不转发**。
两跳都不协商 permessage-deflate（与 codex realtime 一致），消息层保真即
帧 payload 保真。已知协议事实 3/4 ⇒ **帧层零改写**。

**D7.2 下行握手**：hyper `serve_connection_with_upgrades`；atcd 识别
GET + `upgrade: websocket` + `sec-websocket-key`，自算
`Sec-WebSocket-Accept = base64(sha1(key + GUID))` 回 101（hyper `on_upgrade`
取 `Upgraded` 流），`WebSocketStream::from_raw_socket(.., Role::Server)`。
不回显任何 Sec-WebSocket-Protocol（协议事实 3：codex 无子协议）。

**D7.3 上行 URL**：`ATCD_UPSTREAM` 基址按 12.1-2 的 codex 归一规则推导路径，
**入站 query 原样透传**（客户端已按 codex 规则整形 intent/model/call_id，
atcd 不重排、不增删——重排即 URL 字节发散）。v3 路径（`/v1/live`）显式拒绝。

**D7.4 出口绑定（关闭台账 R4 已知缺口）**：persona.proxy_url 有值时先建
SOCKS5 CONNECT 隧道（socks5=本地解析、socks5h=代理端解析；RFC 1929
userpass 支持；自实现 ~90 行，与 HTTP 路径 reqwest socks 行为对齐），
TLS（wss）由 tokio-tungstenite `Connector::Rustls` 在隧道上完成
（webpki-roots，与 codex 同 rustls 栈）。无代理直连。http:// 代理不支持
（本部署 persona 出口均为 socks，遇 http 代理报错退出）。

**D7.5 身份面（B4 在 WS 通道的落点）**：
- 会话键链与 HTTP 相同：`session-id` → `x-session-id`（realtime 恒带其一；
  均缺 → 拒绝升级 400，B3 粘性必须有键，WS 无 body 可兜底）。
- 绑定表/放置/粘性与 HTTP **同一张表同一把键**：同一 codex 会话的 HTTP
  Responses 调用与 realtime WS 必然落同一账号。
- 身份工件透传：session-id / thread-id / x-session-id / turn-metadata 其余
  字段原样上行。
- installation 三投影在 WS 通道的应用：
  ① `x-codex-installation-id` 头 → persona（复用 rewrite::strip_inbound +
  rewrite::apply，与 HTTP 同一函数）；② `x-codex-turn-metadata` JSON 的
  `installation_id` 字段 → persona（rewrite::turn_metadata_with_installation）；
  ③ 帧投影 = **空集**（12.1-4，逐字段依据已列，帧层零改写）。
- 声明偏差：真实 codex v2 握手今日不带 `chatgpt-account-id` /
  `x-codex-installation-id`（12.1-5；realtime 现仅 API key）。atcd 仍写
  persona 值——账号平面一致性优先（与同会话 HTTP 请求呈现同一设备身份），
  且与 codex realtime 接入 ChatGPT 认证的演进方向一致。

**D7.6 生命周期**：升级前完成 ensure_token（复用）→ 绑定/放置（复用）→
`gate.slot` 并发名额**连接期持有** + `wait_turn` 一次（连接建立视作一回合；
帧级不加间隔——音频流不可插入人为延迟，此为与 HTTP 逐回合记账的声明差异）；
上行握手失败：401/403 刷新一次重拨一次（镜像 HTTP 401 自愈），仍失败则
拒绝升级（升级前）或 Close(1011)（升级后）；任一侧 IO 错误 → 对侧
Close(1011)；断开时 touch binding/account（记账一次）。

**D7.7 依赖决策**：tokio-tungstenite 0.27 → **0.28**（openai-oss-forks
tokio 层 fork 版本号即 0.28.0——0.27 patch 因版本错位从未生效，锁文件仍是
registry 源；0.28 起双层真正对齐 codex）。features：
`rustls-tls-webpki-roots`（TLS 栈与 reqwest 的 rustls 同源 0.23）。
不引 tokio-socks（自实现隧道，少一个依赖面）。

**D7.8 验证面**：单测 = URL 归一（对拍 codex methods.rs 测试预期值）、
握手 accept-key（RFC 6455 官方向量）、SOCKS URL 解析、Close code/reason
语义；集成 = mock WS 上游 + ProxyApp 全栈：v2 帧双向**字节级相等**
（含 unicode/转义/未知 type 的刁钻帧与 Binary 帧）、身份头替换断言、
Close code 透传。

**拒绝的替代**：复用 codex-api `RealtimeWebsocketClient` 做上行——其 connect
会代客户端发 session.update（桥必须零语义）；HTTP CONNECT 盲字节代理——
握手层身份改写与账号绑定是 atcd 存在的理由，终结不可避免；tokio-socks
新依赖——REQ：90 行 vs 一个供应链面，不值得。

## 13. D8：养号节律与需求压力的调度（两套时间尺度 + 一个参数）

> 用户提出的矛盾：养号需要"周期地、科学地可持续性竭泽而渔"（长周期像人），
> 而真实需求会爆发（排队请求等不起）。需要一个参数表达"养号多大程度上为
> 排队请求妥协"，并实现带语义的并发（一个账号在一个用户会话下偶尔爆发
> 2~3 个连接，之后有节奏地沉寂）。

### 13.1 概念模型

**两套时间尺度 + 一个参数**：

- **养号的节奏**：账号级状态机 `突发 → 沉寂 → 休整 → 恢复额度`。
  人类的工作模式是「一段专注里连点几下（2~3 个请求搭在一起），然后离开
  一段时间」——突发 + 长尾沉寂，而不是恒定速率。恒定速率本身就是机器特征。
- **排队的等待**：等待中的请求的等待时长。现有架构没有显式队列——
  背压 = 请求在 `wait_turn` 里等待；参数只改变等待时长，不改变排队点。
- **档位 `ATCD_NURTURE_LEVEL ∈ [0,1]`**：养号纪律的保留比例。
  0 = 养号绝对优先（排队请求等满全部沉寂，只有客户端超时能放弃）；
  1 = 需求绝对优先（沉寂立即放弃）；
  中间线性——等待被放弃的比例 = `level`。

### 13.2 状态机（每账号）

```
last_turn     上次回合时刻（既存）
burst_left    剩余突发额度，初始 = B（默认 3）
quiet_until   沉寂截止 = 燃尽时刻 + quiet + jitter
```

判定（一次请求进 `wait_turn`）：

1. **天然休整**（`now ≥ last_turn+gap+jitter` 且 `now ≥ quiet_until`）：
   放行，并且**恢复突发额度** `burst_left = B`（用户休息后回来，又能快速连发）。
2. **突发**（`burst_left > 0` 且 `now ≥ last_turn+burst_min_gap`）：
   放行，`burst_left -= 1`；若燃尽 → `quiet_until = now + quiet + jitter`。
   突发内允许 2~3 个请求在飞（受既有并发闸 `concurrency_per_account`
   的硬界），对应"一次专注操作的连点"。
3. **沉寂**（额度燃尽且未休整）：目标时刻
   `target = max(last_turn+gap+jitter, quiet_until)`，
   实际等待 `wait = (target - now) × (1 - level)`。

### 13.3 设计要点与理由

1. **最小间隔不可让步**：`min_turn_gap`（1200ms）与 `burst_min_gap`（250ms）
   永远执行——亚秒级规律性是最强的机器信号。档位只调节「歇息余量」
   （quiet），不动这两条最小间隔。这是对"多快放弃沉寂"的精确回答：
   放弃的只是沉寂，不是底线。
2. **突发是语义并发**：突发额度允许一阵快速连发，随后沉寂——
   两端都像人：快是因为人在专注操作，慢是因为人离开了。
3. **单旋钮可解释**：档位是一个标量，状态机四态可打印可审计
   （POMDP：状态可见、演化可预测、无隐藏评分）。
4. **与既有并发闸正交**：突发只移除"启动间隔"，不放宽在飞上限
   （semaphore 仍是硬界）。

### 13.4 参数

| env | 默认 | 含义 |
|---|---|---|
| `ATCD_NURTURE_LEVEL` | 0.5 | 养号保留比例（0=养号优先，1=需求优先） |
| `ATCD_BURST_TURNS` | 3 | 突发额度 B（重臂后的连发上限） |
| `ATCD_BURST_MIN_GAP_MS` | 250 | 突发内的最小间隔 |
| `ATCD_QUIET_MS` | 8000 | 燃尽后沉寂基数 |
| `ATCD_QUIET_JITTER_MS` | 3000 | 沉寂抖动 |
| （既存）`ATCD_MIN_TURN_GAP_MS` | 1200 | 休整的最小间隔 |
| （既存）`ATCD_JITTER_MS` | 900 | 休整抖动 |
| （既存）`ATCD_CONCURRENCY_PER_ACCOUNT` | 4 | 在飞硬界 |

保守部署建议 `level ∈ [0.2, 0.3]`（养号偏重）；吞吐压测可临时 0.8~1.0。

### 13.5 拒绝的替代

- **令牌桶恒定速率**：恒定速率本身是机器特征；人类是突发+长尾，不采纳。
- **队列深度反馈调制档位**：增加不可解释的耦合；单参数已满足需求
  （用户原话："决定我们多快放弃账号的沉寂"）。留作后续迭代
  （可加二阶项 `ATCD_QUEUE_PRESSURE`），当前不引入。
- **档位允许突破 min_gap**：见 13.3-1，不采纳——那会让账号行为
  在高压时退化成机器。
- **按会话维度的债务计数**：额度簿记属上游计费平面（B2），atcd 只塑造
  墙钟形态，不做账。
