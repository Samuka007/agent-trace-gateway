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
4. R4 WebSocket V2（codex-api/src/endpoint/realtime_websocket/protocol_v2.rs）
5. 金样本扩容：codex TUI、opencode 多轮、omp
