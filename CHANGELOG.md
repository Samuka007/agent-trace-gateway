# Changelog

本项目的所有显著变更记录在此文件中。

格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### 计划中（台账，未排期）

### 已做（待随下版归档）

- **bench 进 CI**：v0.3.0 已做（ci.yml release 模式真执行，门 264KB ≤700µs）
- **openai.live descriptor 死数据**：v0.3.0 已做（ws.rs 并入 live.rs，TurnMarkers/sse_rules/usage_frames 全部被消费）
- **error 标记扩展**：response.failed/incomplete 落 TurnRecord.error 且 agent+generation 双 span 挂 native status（v0.2.2 已做）；error.type 属性、非流式失败响应与 ws `response.done(status=failed)` 的标记仍待做
- **SseAction::Usage 死载荷**：v0.3.0 已做（变体删除——usage 采集改为帧驱动，顺带修复流式 anthropic usage 从未采集的缺口）
- **req_buf 多次 parse 收敛**：v0.3.0 全量收敛（unpack::turn_facts 单入口：一次描述符查找 + 一次遍历，extract_messages &Value 化）

## [0.3.6] - 2026-09-11

### ① 客户端断开处理策略开关（drain switch）

per-upstream 配置：客户端中途断开时对上游连接的处理策略。

- **`ATG_DRAIN_ON_CANCEL`（默认关）**：
  - **开**（sub2api 类上游——断开也可能照常计费）：继续消费上游流到自然结束——trace 记完整 final_output + 完整 usage；turn 记 `cancelled=true`。
  - **关（默认）**：断开即中止上游请求，不白烧 token；已交付部分照记；turn 记 `cancelled=true`。
- **`ATG_DRAIN_TIMEOUT_SECS`（默认 60）**：drain 窗口。上游流超时未结束则放弃，按已捕获部分记录并打 `drain_timed_out` 标记，防任务悬挂。
- **分类衔接（v0.3.5）**：`cancelled` 位两种模式都打（客户端断开事实）；cancelled turn 非 fail 口径、非 OTLP ERROR level（导出面钉子断言）。
- **内存策略（丢弃转发但不无界缓冲）**：drain 期间停止向下游转发；原始字节捕获到捕获上限为止（溢出如实标记）；SSE 语义内容走增量解析（chunk 边界安全，逐字节切分与整包解析等价钉子），final_output/usage 不受捕获上限截断。
- **机制说明（RustGate 复审后定稿）**：Pingora 响应泵把下游存活与上游消费结构性耦合（下游第一次读写失败即 try_join! 取消上游、丢弃连接），ProxyHttp 钩子内无法实现断开后 drain。**开关关闭（默认）= 全部请求保持 Pingora 泵，零回归**；**开关开启**才把 LLM API 请求（非 WebSocket 升级）交由网关自有 relay 转发（request_filter 短路）：reqwest 上游客户端（`.no_proxy()`——环境代理不劫持数据路径；`ATG_SNI` 在 https 上游时以 resolve 把 SNI 名钉到真实地址，裸 IP https 握手保持可用）+ pingora 下游 session 写出（h1/h2 分帧保持、hop-by-hop 头剥离、缺 framing 头时按泵规则补 chunked）。WS 升级与未知路径在两种模式下都保持 Pingora 泵不变。上游侧断连/半 body 仍归 ProxyError；响应完整交付后的拆除仍归 IdleNoise。
- **已知限制**：①HTTP trailers 不经 relay 转发（reqwest bytes_stream 无 trailer API；LLM API 响应无 trailers，h1 泵本就不转发）；②请求上传在连上游前全量缓冲（LLM 请求量级成本可忽略）；③断开后存活探针复用泵原语 read_body_or_idle(true)——其全部结局（FIN/RST/body 后数据到达）在泵语义里都是会话拆除，relay 同口径记 cancelled。relay 吞吐 smoke 已补（6MB SSE live 转发 + 全量捕获钉子）。

### ② 其他

- **ATG_SNI https 裸 IP 形态**：relay 客户端以 resolve(SNI 名 → 真实地址) 支持 `https://裸IP:端口` + ATG_SNI 的 SNI 覆盖（泵路径原有能力，relay 路径保持）。
- **已知问题（v0.3.5 即存在，非本次引入）**：portless `ATG_UPSTREAM`（如 `https://host` 不带端口）在泵路径 panic（pingora HttpPeer::new 对无法解析地址 unwrap）；README 规定 upstream 需带端口。relay 的 parse_upstream 已支持 portless（默认 443/80）。

## [0.3.5] - 2026-09-11

双特性（RustGate 复审三轮 PASS：idle 基础 → BLOCK 修复 → 三分类方向修正）。

### ① 错误分类学三分：ERROR / CANCELLATION / idle-debug

生产动机：成功请求结束后 sub2api 空闲超时 RST（Os 104，context "during HTTP idle state"）被冒泡为 fail_to_proxy + turn 的 proxy_error——零请求损伤的观测面失真；客户端主动中途断开同样不是网关失败（上游可能已 drain 计费，对账素材）。

- **IdleNoise**（响应完整交付后的拆除）：debug 级噪声，无标记。两判据——(a) Pingora "during HTTP idle state" context 保险带；(b) 结构门：end_of_stream 已到 + 2xx + downstream 源错误（resp_status 只证明头到达，三门缺一不可——防截断响应洗白）
- **ClientCancelled**（客户端中途断开，downstream 源非 idle）：turn 正常记录（部分内容照记）+ `langfuse.trace.metadata.cancelled=true`——**非 fail、非 ERROR level**（对账一一对齐）
- **ProxyError**（上游侧中断/半 body/死连接）：原口径不变
- 判据为结构性 session 方向（错误源 upstream/downstream），非字符串猜测；logging 与 fail_to_proxy 双点接线，respond_error code 路径不变

### ② API Key 指纹（client_key_fp）

- 提取：anthropic.messages → `x-api-key`；openai.* → `Authorization: Bearer`；非 Bearer 方案忽略；无凭据（内部探针）→ 字段缺省不报错
- **公式（可复算）**：`sha256(salt || key)` 的 UTF-8 字节流，结果前 **16 个 hex 字符**
- **salt**：环境变量 `ATG_APIKEY_SALT`（compose 可见可轮换）；未设置时默认 **`atg-apikey-fp-salt-v1`**
- **CLI 复算**：`gateway key-fp <api-key>`——与运行时同一代码路径（同一 pub fn），输出即 trace 里的 `langfuse.trace.metadata.client_key_fp`，可直接当 Langfuse 过滤值
- 明文边界：原始 key 在提取边界即弃——任何日志/records/health/导出面均无明文（records JSON 钉子断言）
- 同 key 跨两协议入口同指纹；不同 key 不同指纹（E2E 钉子）

### ③ 兼容性

- 无破坏：新字段（cancelled/client_key_fp）均为增量 metadata；现有查询/过滤不受影响
- 消费侧新增可用过滤键：`cancelled=true`（对账视图）、`client_key_fp`（key 复用关联）

## [0.3.4] - 2026-09-11

**trace 形状改 GENERATION-only**（用户裁定，RustGate 复核 PASS）。

### 动机

转发网关 1 请求↔1 调用——"一请求多 LLM 调用"在设计边界内不存在；AGENT 容器承载的分类信息已在 metadata.harness（更准确），容器节点是零信息冗余；ATG 哲学=不为假想需求留结构。trace 根直接是 type=GENERATION 的 generation（Langfuse OpenAI/LangChain 单调用集成的根 generation 惯例；observation 名保留命名空间化的 agent.turn.generation，trace.name 仍 agent.turn）。

### 变更

- 单一 span/turn：原 agent 容器 span 与 generation 子 span 合并为根 generation（无 parentSpanId）
- 字段迁移核对零丢失：observation.input/output（mapping 表允许任意 observation）、AMB-7 ERROR status、turn 时长、usage/model/completion_start_time 上根；trace metadata（harness/dialect/client_ua/entry_protocol/client_model/...）与 session/user/tags 不变
- OBSERVATION_TYPE_AGENT 常量与容器发射路径 clean cutover 删除
- 测试全量重写（单 span 断言 + no-container + P0-N1 单 span 重述）；语义审计文档加形状修订注记

### 兼容性（重要）

**消费侧按 `type=AGENT` 过滤的查询会失效**——分类过滤应改用 `langfuse.trace.metadata.harness`（identity 层，v0.3.2 起语义更准）。生产 Langfuse 尚未接入，正是改形状的窗口。

## [0.3.3] - 2026-09-11

双批次小修（RustGate 复核 PASS）：harness 归因健壮性 + frame 计数语义。

### 归因健壮性（生产 anthropic 线误归因排查产物）

- **UA 前缀匹配大小写不敏感**：`IdentKind::ua_matches`——OMP/18.1.16 此前完全错过 omp/ 前缀（生产 candidates=['claude-code'] 单元素签名的可能成因）；三大小写钉子 + legacy fallback 负控制
- **多字节 UA panic 修复（安全级，RustGate BLOCK）**：`u[..prefix.len()]` 字节切片在字符中间 panic——UA 客户端可控 + panic 落 logging 钩子 = 恶意 UA 杀连接（DoS 面）。改 `u.get(..prefix.len())`（边界外返回 None=不命中不 panic）；'日éx/1.0 omp'（byte 4 落 é 内）与 'ÖMP/18.1.0'（ASCII-fold miss）钉子
- **langfuse.trace.metadata.client_ua**（新）：网关实际所见的 UA 原文（字符边界截 256）——归因争议的 ground truth，不再跨库 join 猜
- 生产排查记录：v0.3.2 栈上三形态实测（plain/count_tokens/streaming-envelope + omp UA + CC header）全部正确归因 harness=omp；误归因残余差异=anthropic 线到达 ATG 的 UA 实际形态，client_ua 下次捕获直接落证

### frame 计数语义

- **frame_errors 跳过协议合法非载荷帧**：SSE 注释 keep-alive（`:` 开头/空 data 载荷）与 OpenAI chat `data: [DONE]` 终止哨兵不再误计为解析错误（G3 可观测基础必须语义干净）；真坏帧仍计数。钉子：comment_and_done_frames_never_count_as_errors

## [0.3.2] - 2026-09-10

双批次（RustGate §G 双 PASS r2）：harness 两级归因 + 宽松路径匹配。

### 两级归因：dialect 与 identity 解耦（生产误归因修复）

omp 以 claude-code 方言发 anthropic messages（x-claude-code-session-id + CC 形态 metadata），此前被整批标 harness:claude-code。现在：
- **identity 层**（metadata.harness / tags harness:*）：身份独占证据——UA 前缀（claude-cli/、omp/、codex、opencode/）、codex x-codex-* body 指纹、CC legacy 复合形态（≤2.1.114 指纹）；identity 类证据优先于一切形态证据（跨 strength）。
- **dialect 层**（新独立键 metadata.dialect）：会话携带形态规则集（CC envelope/legacy/object/metadata-envelope + CC header）——借用方言是常态，omp 说 claude-code 方言但身份仍是 omp；session 提取按形态键控零漂移。
- 无 identity 证据但方言形态命中 → 降级断言 "<dialect>-compatible"（不算本体）；CC 本体收紧：claude-cli UA 或 legacy 形态，纯 header/envelope 命中不再冒充本体。
- 新增 **omp descriptor**（UaPrefix omp/ s=4，协议=实测三形态，宁缺勿造不含 live）；turns_with_harness 只计 identity 层。

### 宽松路径匹配 + 上游门控（omp base URL 不带 /v1）

omp 配裸 host 时 POST /responses 检测不命中→透明转发无 trace 且静默。现在：
- **表驱动 loose_endpoints**（responses / messages / chat/completions；live 无）：无精确前缀命中时**尾锚定**匹配——端点段序列必须是路径结尾或延续进白名单子资源（count_tokens）；/api/messages/list 类业务路由不匹配。compatible-mode/v1/* 精确规则不变。
- **detect_path 两级返回**（精确/宽松带 loose 标志）；**记录门控**：宽松命中仅上游 2xx 才记录（404/5xx 不产假 turn）；精确命中行为完全不变（错误 turn 照记）。
- **可观测**：/__atg/health 增 loose_path_matches 计数 + 宽松命中首次采样日志（path+protocol，配置排障）。

### 验证（CT104，--jobs 6）

fmt+clippy clean；dialect 73 测试（harness 15 钉，含 BLOCK-G1 恢复的 object/metadata-envelope 提取钉）；loose 74 测试（protocol 20 钉 + E2E 四场景：/responses 200 记录、/responses 404 门控+计数、/v1/responses 404 精确回归照记、/chat/completions 变体记录）。

## [0.3.1] - 2026-09-10

E2E NO-GO 修复批次（.tmp-e2e-otel-v030.md，RustGate 复审 PASS）+ 台账 NIT 两项。

### 修复

- **P0-N1（阻塞项）**：agent span 与 generation span 各自 `random_trace_id()` → 同一 turn 落两条 trace、parentSpanId 悬空（P0-1 随机化引入，确定性 id 时代共享值掩盖了此缺陷）。现在每 TurnRecord 生成一次 traceId，两 span 复用；spanId 保持各自独立随机。钉子：same_turn_spans_share_one_trace + otlp_export E2E wire 断言
- **G1**：全零 usage 不再导出空对象 `usage_details="{}"`——属性整体省略（零值 ≙ 未报告）。钉子：all_zero_usage_omits_usage_details
- **G2**：非流式 tool_calls 提取落地（descriptor 新字段 nonstreaming_tools：anthropic content[].tool_use / responses output[].function_call+custom_tool_call（arguments|input 双读，与流式 ToolDone 对称）/ chat choices[0].message.tool_calls[]；live=None 由 WS 组装自有）。钉子：nonstreaming_tools_extract_from_all_three_shapes
- **G3**：HTTP/代理层失败置 TurnRecord.error——`http_status: <code>`（≥400）或 `proxy_error: <e>`，协议级 error marker 优先；NIT-B：响应体不可解析时发**最小错误记录**（此前该 turn 整个消失）。E2E：JSON-404 与 text-502 两探针

## [0.3.0] - 2026-09-09

三层架构 reconcile（model / protocol / harness / trace 四层，Cargo workspace 编译期强制依赖红线）+ harness 能力。API 语义兼容（OTLP wire 仅新增属性）；行为变更逐条枚举于各 commit message。

### 结构（F1）

- **Cargo workspace 四 crate**：`atg-model`（Langfuse 语义词汇 + TurnUsage/TurnRecord，零协议零 harness 知识）、`atg-protocol`（descriptor 拆 `anthropic/` + `openai/{responses,chat,live}` + mounts 挂载点常量 + session/usage 求值器）、`atg-harness`、gateway 主 crate（trace 编排 + src/engine.rs 薄解释器）。红线 = Cargo.toml 事实：protocol→model、harness→{protocol,model}、model 零内部依赖。
- **harness/fixture_server.rs → tests/common/**：dev-only mock 移出产品二进制；hyper/tokio-tungstenite/futures-util/sha1_smol 等移入 [dev-dependencies]。

### harness 层（F2，实证设计 §1/§2/§5）

- **4 个 HarnessDescriptor**：claude-code（envelope ≥2.1.22x 主流 + legacy + object/metadata-envelope 兼容形态；legacy account 段 → `langfuse.trace.metadata.cc_account`）、codex（client_metadata x-codex-* 指纹 + installation-id enrich）、grok（x-grok-conv-id，仅 grok-route 合法）、opencode（UA + x-session-* affinity 家族，attribution 门控——未识别客户端不能凭这些 header 铸 session）。
- **strength 有序识别**：指纹（3-5）先于 UA（4）；同级冲突记 `harness_candidates` 不强消歧；协议合法性是声明——越界命中记 `harness_protocol_anomaly`。**分类失败不降级 session 提取（红线）**：session body 规则按形态命中运行，与归因结果正交。
- **session 迁移映射（§5）**：来源 4/5/6/7（CC user_id 形态）归 harness session 规则，1/2/3/8/10 留 protocol 表；anthropic body_sources 收缩为两个通用挂载点，有效优先级不变（E2E fixture 钉住）。
- **wire**：`langfuse.trace.tags += harness:<name>`（两 span）+ `langfuse.trace.metadata.{harness,harness_candidates,harness_protocol_anomaly,cc_account,codex_installation}`；`/__atg/health` 新增 turns_total/turns_with_session/turns_with_harness 图景计数（分母过滤查询侧）。

### 行为变更（用户裁定，F3/F6）

- **chat 退出 prefix 指纹兜底**：chat_completions SDK 流量无会话语义，不再合成 session（`stitch_eligible=false` 数据驱动）；**链长 ≥2 才合成**——单消息无连续性证据；**合成 session 一律 `langfuse.trace.metadata.session_synthetic=true`** 且不入命中率分子。受影响测试（prefix_stitch/restart_stitch/replay_calibration/bounded_state）显式重写并注明裁定。
- **流式 anthropic usage 缺口修复**：usage 采集从 Usage-action 门控改为帧驱动（usage_frames 逐帧）——此前流式 anthropic usage 从未累积（表无 Usage action），现在 message_start/message_delta 正确合并。SseAction::Usage 死载荷随之删除。

### 修复与能力（F4/F5/F6）

- **ws.rs 并入 protocol/openai/live.rs**：WsFrameParser/WsTurnState 与表同文件，事件名全部从表读取（TurnMarkers + sse_rules + usage_frames——死数据批评以消费偿还）；take_record 从 response.create 帧补 model/user。
- **P1-8** `langfuse.observation.completion_start_time`（仅 generation，ISO 8601 Z 纳秒精度，std-only 实现）：取**线上首输出字节时刻**（首个 SSE body chunk / 每 turn 首个 WS 服务帧，Ctx 记录逐 turn 重置；非流式 = 请求起点）。
- **P1-10** `langfuse.trace.metadata.{entry_protocol,client_model}`（两 span，键名与 modeltrace 线对齐）。
- **F5 bench 进 CI**：`cargo test --release --test sse_bench` 真执行，门 264KB ≤700µs（实测 372-435µs，~2× 裕量）；fmt/clippy/test 升级 --all/--workspace 覆盖成员 crate。
- **死代码清偿**：`trace_id_for`/`span_id_for` 空壳 shim、`SseAccum::finish`、`SseAction::Usage` 变体、`GEN_SPAN_ID_SEED`；`unpack::turn_facts` 请求侧单入口（一次描述符查找一次遍历，extract_messages &Value 化——req_buf 二次 parse 清零）。

### 验证（CT104 @ 全部阶段绿）

fmt --all --check + clippy --workspace --all-targets -D warnings clean；全量 67 测试绿（gateway 14 + harness 13 + model 2 + protocol 18 单测 + 20 集成）；release bench：SSE 264KB **435.6µs**（门 700µs）、anthropic 3000 帧/50 工具 2.82ms。

## [0.2.2] - 2026-09-09

Langfuse 官方语义对齐：审计 P0 五项 + 全部语义歧义（AMB）清零。数据路径（SSE 单遍零拷贝）零改动、零新依赖；RustGate 守门累计五轮放行（§D 阶段 4 轮 + §E Round 2/3/4 复审，Round 4 PASS 零 BLOCK 零歧义）。

### 修复（Langfuse 语义）

- **P0-1 trace/span id 随机化**：确定性派生 id 在 Langfuse spanId upsert 下 85% 吞数、长会话巨型 trace——改 per-turn 随机 id（`replayed_turns_get_distinct_ids` 反向钉子），会话归组唯一靠 `langfuse.session.id` — `126272b`
- **P0-2 usage 互斥桶**：OpenAI inclusive input（cached_tokens 与 cache_write_tokens 均含于 input）生产侧派生为 exclusive 桶（input − cache_read − cache_creation，saturating clamp）；官方依据：OpenAI cookbook per-run spending controller（ordinary = input − cached − written）+ Langfuse 归一化表（flat `langfuse.observation.usage_details` "stored unchanged; values must already be exclusive"）— `126272b`、`4913756`
- **P0-3 observation.input/output**：agent 根 span 发官方内容键 `langfuse.observation.input/output`（空串省略，空输出噪声一并消灭）— `126272b`
- **P0-4 session 传播 + AMB-6**：`langfuse.session.id` 复制到 generation 子 span（observation 级过滤可见 usage）；双拼写收敛为官方单键（负向钉子防回潮）— `126272b`、`def4793`
- **P0-5 model.name**：`langfuse.observation.model.name` 挂 generation 子 span（TurnRecord.model_name，单次 req parse 填充，空则省略）— `126272b`
- **P1-6 langfuse.user.id**：agent+generation 双 span 非空发射（R2-1，死字段落地）— `def4793`
- **P1-7 ingestion header**：`x-langfuse-ingestion-version: 4`（v4 实时摄入）— `126272b`
- **R2-2 单次 parse**：logging 钩子 req_buf 单次 parse 共享给 session/user_input/model/end_user 提取器（C14 req 侧纪律）— `def4793`
- **AMB-1**：TurnUsage 文档改派生互斥桶语义（input_tokens 已减 cache，防 records/导出消费者二次扣减）+ usage_from_obj 减法注释引官方依据 — `4913756`
- **AMB-2**：usage_details 跳过 0 值条目（0 ≙ 未报告；Langfuse `total` 按在场桶求和，省略无损，消灭 `{"input":0,"output":0}` 空转噪声）— `4913756`
- **AMB-7**：errored turn 的 native OTLP status（code=2 + message）同时挂 agent 与 generation span（Langfuse level/statusMessage 按 span 映射，generation 过滤的错误视图不缺数据）— `9e025ec`
- **incidental**：chat ToolChunk name 首次命中即定（重发 name 的兼容网关不再拼出重复名）；删除零调用者 `extract_end_user`/`extract_model_name` — `680f2f0`

### 验证

CT104 实跑（RustGate 本机独立复跑数字一致）：lib **33/33**（+3 新钉子：usage_details_skips_zero_entries / errored_turn_marks_both_spans / chat_tool_name_first_hit_wins）、clippy `--all-targets -D warnings` clean、全量 debug 套件 **33+20 全绿**；release bench（数据路径与 v0.2.1 相同、零回归）：SSE 264KB 重组 435µs、anthropic 3000 帧/50 工具 4.42ms（CT104；bench 断言全过。v0.2.1 所引 372µs 为 RustGate 主机，数字机器相关）。

## [0.2.1] - 2026-09-08

内部重构 + 修复 + 性能门禁（无 API 语义变化）。RustGate 审阅 4 轮（初审 9 BLOCK → 复审2 清 4 → 复审3 残 1 → 复审4 PASS）。

### 变更

- **ProtocolDescriptor 架构**：协议知识收进编译期数据表（`src/trace/descriptor.rs`，anthropic/responses/chat/openai.live 四表）+ 唯一解释引擎（`src/trace/engine.rs`）；unpack/session/adaptor 的协议 match 全删（unpack −302 行）。会话来源按语义分层（Root > StableAffinity `pck:` > ResponseChain(跳过) > EndUser）— `998d72f`、`1c8e3f1`

### 修复

- **openai cache tokens**（slop#3）：`*_details.cached_tokens` 嵌套路径表驱动解析（responses/chat/live 三钉子测试）
- **CRLF SSE 分帧**（BLOCK-A）：行驱动分帧使 LF-LF 与 CRLF-CRLF 分隔符等价——CRLF body 经规范化代理不再塌缩单帧丢流；RustGate 探针收编 `tests/crlf_framing.rs` — `bcf6bc8`
- **frame_errors 可观测**（BLOCK-B）：reassemble_sse 透传 frame_errors → Gateway.failed_frames 计数 + `/__atg/health` 暴露 + 采样日志（首条+每 10 条）— `bcf6bc8`、`3595791`
- **错误标记**（slop#6）：response.failed/incomplete + anthropic event:error 落 `TurnRecord.error` — `da69ee9`
- **responses final_output**：跨 item 拼接全部 output_text（修复多块丢正文）— `1c8e3f1`
- **zero-copy SSE 帧**：UTF-8 合法路径借用 body（修复 a3ec31e 空提交欠账）— `bcf6bc8`

### 性能

- descriptor 引擎 micro-bench（`tests/sse_bench.rs`，release 3 轮中位）：SSE 264KB 文本重组 **372µs**（v0.2.0 基线 1.7ms）；anthropic 3000 帧/50 工具流 2.78ms — `53864d8`

## [0.2.0] - 2026-09-08

首个带 tag 的发布基线。fork main 自 v0.1.0（`df0d32a`）以来的 5 项语义变更，全部经本地 dirty E2E 双线对齐验收（ATG vs sub2api modeltrace，ALL PASS）。

### 新增

- **Langfuse adaptor**：usage 提取与 generation 子 span（agent.turn + generation 两级 span、usage_details、无 gen_ai.* 前缀）— `3eef71c`

### 修复

- **OTLP 词表对齐 sub2api modeltrace**（G1–G4，Langfuse 双跑轨迹可比）— `eb0ecde`
- **OTLP endpoint 处理**：裸 host 自动补 `/v1/traces`；导出失败记录日志（不再静默）— `1f06b99`
- **Anthropic tool_use**：content_block 提取为结构化 `tool_calls` 属性（`8f64b1a`）；`content_block_delta` wire format 对齐，input_json_delta 真正命中（`72c7ba7`）
- **OpenAI Responses 裸 input items**：无 `type` 字段的 item 按 role 提取 user_input — `9a53517`

### 已知开放项

- openai_responses 协议 turn 记录 gap（usage few-shot 2/3 PASS），修复已转 GapFix lane。

## [0.1.0] - 2026-09-08

初始版本（`df0d32a`）：Pingora 反代 + 三协议 SSE 重组 + prefix 拼接 + OTLP 导出。
