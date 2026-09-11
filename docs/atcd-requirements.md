# atcd 需求台账

> 规则：每个需求先进本账，再动工。状态只有四种：`待办 / 进行中 / 已完成 / 冻结`。
> 完成必须附证据（commit、测试名、冒烟输出）。历史需求不因新需求加入而失效，
> 除非在此文件中显式标记"覆盖/放弃"并写明理由。
>
> 过程规则（用户指令）：新需求入账即排队，**不立刻切换执行方向**；
> 执行顺序以本表"待办顺序"为准，当前工作项完成并提交后才进入下一项。

## 产品边界（不变量）

| 编号 | 内容 | 状态 |
|---|---|---|
| B1 | 仅接受 Responses API；body 透传优先，改写必须逐字段列举并有源码依据 | 生效 |
| B2 | 下游仅 coding agent；无用户/计费/分组面（这些归 newapi） | 生效 |
| B3 | 会话粘住账号，永不迁移；压力下冻结或作废 | 生效 |
| B4 | 身份工件（session/thread/window/turn，uuid v7、真实时戳）透传；仅 installation（账号级）在三个投影点重写 | 生效 |
| B5 | 不提供"重铸人设"类能力；installation 与账号同生命周期 | 生效 |

## 需求清单

| 编号 | 需求 | 来源 | 状态 | 证据/备注 |
|---|---|---|---|---|
| R0 | 开 worktree 完成最简原型（六件套：透传代理/会话表/放置/节奏/刷新/导入），能引用 codex crate 就直接引用 | 用户指令 | 已完成 | a1e6650 |
| R0.1 | 身份层重构：UA 按 codex 源码格式（originator 前缀 + os_info + 终端探测）、id 全部 v7、透传优先 | 用户纠错（"有没有检查过 UA/session/thread 生成逻辑"） | 已完成 | 1466663 |
| R1 | OAuth 登录：device-code（codex 原生远程路径）+ 粘贴回调 URL 模式（web 形态雏形）。决策记录：先引用 codex-login → 其硬依赖 native-tls/openssl 在 nix 环境无法链接 + tungstenite 双层 fork 解析冲突 → 撤引用，wire 逐字镜像（pkce.rs / server.rs:584-606,809-843 / device_code_auth.rs） | 用户需求 | 已完成 | oauth.rs + `atcd login [--device]`；交换/解析有单测 |
| R2 | 账号管理：配额头捕获落库（x-codex-primary/secondary-used-percent）、放置配额天花板（ATCD_QUOTA_CEILING_PERCENT，默认 85）、enable/disable、富列表 | 用户需求 | 已完成 | `atcd accounts/enable/disable`；冒烟显示 5h%=42 7d%=7 落库 |
| R3 | 其他 responses 原生 agent（omp/opencode）支持：无 codex 身份头时铸造 v7 身份树 + 合成 turn 元数据（17 字段全集），body 信封合成（store/include/prompt_cache_key=铸造 session/client_metadata/键序对齐金样本），会话键 prompt_cache_key → body 前缀 sha256 兜底；instructions/工具表保留其自洽家族 | 用户需求（已确认 omp 支持 Responses） | 已完成（R3a 修法 A 已实现：cache key=铸造 session） | 冒烟路径B：body 信封全字段落位；opencode 金样本（R7a）实证：无 codex 身份头（铸造路径必要），prompt_cache_key=ses_ 会话 id 且两轮稳定，样本 scripts/fixtures/opencode_1.18.29.{headers.json,body.json}+_turn2.body.json。**R3 残余缺口实证（2026-09-11，三金样本只读分析）——①input 内嵌身份：codex_exec 样本 input[0..2]（message items）均携带 `id` 字段，形态 `msg_<uuid v7>`（如 input[0].id="msg_01a08ee3-a0fe-79c1-8669-5711f556496b"，其 v7 时间戳前缀与同请求 session-id/prompt_cache_key 01a08ee3-a004 同批生成）；fork 源码证实该 id 为**客户端本地生成**：protocol/src/response_item_id.rs:20-22 `ResponseItemId::new = {prefix}_{Uuid::now_v7()}`、items.rs:648-649 主动配 `Some(ResponseItemId::new("msg"))`，且 response_item_id_tests.rs:25-28 表明上游语义视 id 为不透明字符串（服务器 id 逐字接受，prefixed 与否可区分）——非上游签发的存储句柄；opencode（t1/t2）与 omp 的 input item 键集仅 {content,role}，零 id/call_id/encrypted_content/UUID（全树 msg_/rs_/fc_/item_/call_ 前缀正则 + uuid 扫描排除）；三家顶层与树内均无 previous_response_id；store:false 下上游无服务端存储可对账，atcd 内容平面零改写（codex 路径仅 installation 字符串外科替换、omp 路径信封合成仅动 prompt_cache_key/client_metadata/顶层键序，rewrite.rs codex_envelope_body 不触碰 input）+ B3 粘性（同会话恒回同账号）⇒ 现行透传安全，无需改行为。样本未覆盖的形态（function_call_output 引用 call_id 需与同请求 function_call 配对、reasoning encrypted_content 重放需同账号解密）在真实客户端全量重放历史语义下自携带配对/同账号，且同样被"零改写+粘性"保护，残余风险仅剩客户端自身发送破损历史（非 atcd 职责）。②tools schema 深度差异：codex 基准 10 工具=4 种类型家族（8×type:function 显式 strict:false + type:custom apply_patch + type:tool_search + type:web_search），parameters 统一 {type:object, properties, required, additionalProperties:false}，嵌套最深 9 层（request_user_input），required 可空数组（get_goal），关键字全集 type/properties/required/description/items/enum/additionalProperties，无 $schema/$ref/oneOf/allOf；opencode 9 工具全部 type:function+strict:false，parameters 缺 additionalProperties，最深 6 层（todowrite），字符串 `format` 系属性名（webfetch.properties.format）非 JSON Schema format 关键字（已定位排除）；omp 12 工具全部 type:function，**strict 字段缺省**（Responses API 默认 strict=false，与 codex 显式 false 同语义），parameters 带 additionalProperties:false，最深 9 层（task，与 codex 上界持平），含 anyOf（task…outputSchema.anyOf）与 default（task…agent.default）——均非 strict 模式合法关键字，strict 封口校验（required 全覆盖/additionalProperties:false 强制）仅在 strict:true 时触发，三家无一处 strict:true；三家工具名全部 `^[a-zA-Z0-9_-]+$` ≤64 字符（实测最长 18）。⇒ opencode/omp 工具形状是 codex 金样本形状的真子集（仅 function 类型；关键字 ⊆ codex 用集 ∪ {anyOf,default}），无会被上游 schema 校验拒绝的字段形态，R3"保留自洽家族"策略维持，无需改 atcd 行为。附加发现（两问范围外）：omp 请求缺 tool_choice 字段（codex/opencode 显式 "auto"；上游默认 auto，无功能风险，R7b 未记录）；opencode/omp 均无 parallel_tool_calls 字段（codex 显式 true）；opencode developer 消息（input[0]）content 为裸字符串而非 content 数组（Responses API 合法形态）。 |
| R3a | 【R3 开放问题→已决策】cache key 与 session-id 一致性：采用修法 A（prompt_cache_key 重写为铸造 session_id，对齐"cache key 派生自 session"的真实不变量），信封合成实现 | 自查发现 | 已完成 | rewrite.rs codex_envelope_body + 单测；冒烟路径B |
| R4 | WebSocket V2 支持 | 用户需求 | 已完成 | 设计 docs/atcd-design.md §12（D7，全部标注 codex 源码行号）；实现 src/atcd/ws.rs + proxy.rs route 分发 + rewrite.rs sec-* 剥离；依赖 tokio-tungstenite 0.27→0.28（0.27 时期 tokio 层 fork patch 因版本错位未生效，0.28 起双层锁 fork git 源、与 codex 同 rev）；**已知缺口（上游 socks 绑定）已关闭**：persona.proxy_url → SOCKS5 CONNECT 隧道（socks5/socks5h + userpass），e2e ws_bridge_dials_upstream_through_socks5_exit 证明；声明偏差：①v2 帧无 installation 投影点（类型全集核对）→ 帧层零改写 ②上行握手头复用 rewrite::apply 全集（含今日 codex v2 不发的 chatgpt-account-id/installation 头，账号平面一致性优先）③连接建立记账一回合、帧级不加节奏间隔。证据：容器 build EXIT=0 + test EXIT=0（50 passed/0 failed，2026-09-11）；tests/ws_v2_bridge.rs 两用例（字节级保真+身份断言 / SOCKS 出口） |
| R7 | 真实下游流量捕获（金样本）：codex 0.153.4 custom provider (wire_api=responses) 模式实测，nix 提供二进制 | 用户审计提问（"你真的捕获过下游流量吗"） | 已完成（首个样本） | scripts/fixtures/codex_exec_0.153.4.{headers.json,body.json}；发现：①身份头全套存在（session-id/thread-id/window/turn/β-features），codex_native 判定成立 ②session_id==thread_id（exec 模式）③turn 元数据 17 字段（agent_name/context_window_id/request_kind/sandbox 等远超早期假设）④originator 随表面变化（exec=codex_exec），UA 带 "(codex_exec; 0.153.4)" 后缀 ⑤os_info 渲染第三次验证（NixOS 26.11.0）⑥流断自动重连 5 次 ⑦已据此升级 mint_turn_metadata 全字段集 + session==thread 对齐 |
| R7a | 金样本扩容（opencode）：responses wire 实测捕获（nix opencode 1.18.29；capture_opencode.sh 单轮实测跑通，fixtures 取 /tmp 改造版两轮驱动的同会话配对样本），对照 codex 金样本出方言差异清单 | 用户需求（台账待办 5） | 已完成 | scripts/fixtures/opencode_1.18.29.{headers.json,body.json} + opencode_1.18.29_turn2.body.json（turn1/turn2 同会话，prompt_cache_key 相等且==x-session-id）；方言差异 vs codex：①headers 无任何 codex 身份头（无 originator/session-id/thread-id/x-codex-*），代之 Bun 侧 x-session-affinity/x-session-id（=ses_ 会话 id）+ UA "opencode/1.18.29 ai-sdk/provider-utils/4.0.38 runtime/bun/1.3.13"，Accept:*/*（codex 为 text/event-stream）→ codex_native 判非 native 成立，铸造路径适用 ②body 无顶层 instructions（系统提示在 input[0] role=developer）、无 client_metadata；新增 max_output_tokens:32000；reasoning 多 summary:"auto"；store:false/include/stream/tool_choice/text.verbosity 与 codex 同值 ③prompt_cache_key=opencode 会话 id（ses_…），两轮请求完全一致（turn2 实证连续性）；title 副请求不带 prompt_cache_key ④input 内容为 parts 数组（input_text/output_text），assistant 历史（"ok"）客户端全量重发，无 previous_response_id——与 codex store:false 同构 ⑤每条用户消息附带一条 title-generator 副请求（独立 developer 提示、无 tools/prompt_cache_key/reasoning/text 字段），网关需容忍 ⑥工具表 9 个自洽家族（apply_patch/bash/glob/grep/read/skill/task/todowrite/webfetch），证实 R3"保留自洽家族"决策 |
| R7b | 金样本扩容（omp）：omp 18.1.16 responses wire 实测捕获（本机 models.yml 临时加 capture provider → 127.0.0.1:8499 mock，跑后即还原；用户批准） | 用户指令（"加provider不影响，直接加直接测就行"） | 已完成 | scripts/fixtures/omp_18.1.16.{headers.json,body.json}（body 170KB=完整系统提示+12 工具）。方言差异 vs codex：①headers 无 codex 身份头，UA omp/18.1.16，Accept: text/event-stream（与 codex 同）②prompt_cache_key=omp 会话 ULID（01a09155-…，原生发送）③instructions 顶层 117KB 在场（同 codex 形态，与 opencode 异）④无 client_metadata；reasoning={effort:high, summary:auto}（同 opencode）；max_output_tokens 在场（同 opencode）⑤tools 12 个自洽家族（read/bash/edit/eval/glob/grep/task/hub/web_search/write/learn/manage_skill）⑥R3 结论 reinforced：omp/opencode 均原生带会话级 cache key + 无身份头 → 铸造路径与信封合成的映射规则成立 |
| R11 | E2E 真实 agent 流量 + 观测分析：三下游（codex exec / opencode / omp）经 atcd 打向 fake 上游（mock SSE），双端捕获做改写差异分析（身份头/信封/cache key/内容平面保真）+ atcd 侧观测（日志、绑定表），产出流量分析报告。login 实现按用户指示封存（R10a 实测已裁决语义，真实账号完整登录待有号后做） | 用户指令（"面向上游做一些 agent 的调用以及结合观测做一些流量上的分析"） | 已完成 | report.md（282 行）+ evidence/ 21 对双端捕获：内容平面 21/21 逐字节保真，仅身份面改写；prompt_cache_key 三态对照齐；绑定表+日志证据；4 项观察上报（§9，含 opencode 路径身份树缺失的设计张力、omp retry-vs-new-turn 不可分辨） |
| R1a | 依赖对齐决策记录：曾按"依赖以 codex 为准"引入 codex-login + 双层 tungstenite fork patch → codex-http-client 硬依赖 native-tls/openssl，nix shell 链接失败（rama 全系需 pin alpha.4 亦已处理）。reqwest 已切 rustls（对齐 codex TLS 栈）；tokio-tungstenite 维持 0.27（仅测试用帧解析）。若未来换有 openssl 的构建环境，可重启 codex-login 引用 | 用户指令 + 环境 blockers | 已完成（决策关闭） | Cargo.toml 注释 + 本行 |
| R5 | （冻结）存量账号 installation 迁移断崖的错峰方案 | 用户指示"列到 future dream" | 冻结 | 接生产池时再启 |
| R6 | （冻结）vendor 栈跟随 codex 升级的维护节奏 | 用户指示"列到 future dream" | 冻结 | 同上 |
| R8 | CI 迁移 nix 工具链并充分利用 runner：现 ci.yml 用 ubuntu-latest + rust:1-bookworm 浮动工具链，且缺 libssl-dev（codex 依赖树 native-tls 必挂）；目标 = 与 dev 同源（flake/fenix 1.95.0），评估 self-hosted runner 可用性 | 用户指令（"ci.yml 使用 nix 工具链…让 subagent 去看看怎么充分利用 runner"） | 已完成 | ci.yml 重写（nix develop 三步 + nothing-but-nix 磁盘保障 + rust-cache，SHA 钉死）；actionlint 0 findings；CI 命令序列容器实测全绿；runner 事实（两 remote 无 self-hosted、sub2api fleet org-scoped 跨仓不可复用）与 self-hosted 备选模板留档 .scratch/atcd-ci-nix/。注：首跑前置 = worktree 需过 cargo fmt（与 R4 WIP 一起收敛后统一提交） |
| R9 | fork 补丁自动化：维护 Samuka007/codex@atcd-libs 相对 openai/codex 的 patch list，自动 apply 到 latest upstream 并验证（构建测试），产出可重复执行的同步工具 | 用户指令（"fork然后维护一个patch list自动给latest upstream patch到fork去"） | 已完成 | scripts/fork-sync + PATCHLIST.md（2 补丁均为可见性翻转）；atcd-libs-sync-20260911 重放验证通过，未 push |
| R10 | OAuth web/PKCE 远端登录流研究：codex 自身跨设备登录（VPS 发起、另一设备浏览器授权）的真实流程与 atcd web 形态差距评估（R1 已有粘贴回调雏形） | 用户指令（"研究一下…他是走的什么流程"） | 已完成 | .scratch/atcd-r10-oauth-web/research.md（全部结论带源码行号）。硬发现：codex client_id 的 redirect 白名单仅 localhost ⇒ web 形态只能粘贴模式或 device 代轮询；device 流 PKCE 对由授权服务端生成随轮询下发（能轮询即持 verifier）。**衍生实测项 R10a**：atcd poll_device_code 以 400 为 pending（oauth.rs:264-266）vs codex 以 403/404 为 pending（device_code_auth.rs:131）——必有一方不符真实服务，需真实登录实测裁决；bin/atcd.rs:304-306 过时注释待清理 |

## 已证伪/已废弃的认知（防回潮）

| 内容 | 结论 | 证据 |
|---|---|---|
| "Ubuntu 22.4.0 是 sub2api 的笔误、最硬签名" | 错。os_info 把 22.04 渲染为 22.4.0，真实 codex 就是这样 | os_info version.rs Semantic Display；本机 NixOS 26.11 → 渲染 26.11.0 |
| "UA 前缀是 codex-tui" | 错。前缀是 originator（codex_cli_rs）；codex-tui 只是兼容列表旧值 | login/src/auth/default_client.rs:164-175 |
| session/thread/turn 用 uuid v4 | 错。SessionId/ThreadId/turn_id 均为 Uuid::now_v7 | protocol/src/session_id.rs、thread_id.rs；core/src/turn_metadata.rs:96 |
| sub2api 式"收敛成 1 设备 1 会话高强度"更像人 | 错。社区与源码证据：多设备正常，单设备单会话持续高强度不正常 | linux.do 2854867；turn_metadata 字段语义 |
| "codex 固定 parallel_tool_calls:false" | 错。金样本实测 parallel_tool_calls:true | codex_exec_0.153.4.body.json |
| "codex 工具是 shell/apply_patch/update_plan" | 过时。0.153.4 实测：exec_command/write_stdin/apply_patch/view_image/get_goal/create_goal/update_goal 等 10 个 | 金样本 body |
| "reasoning 含 summary:'auto'" | 不完整。0.153.4 实测仅 {effort:"medium"} | 金样本 body |

## 构建与验证状态（随提交更新）

- 分支：feat/atcd-minimal-proxy（worktree .worktrees/atcd）
- 最新提交：ea7284d（本批三连：09a4867 D6+依赖对齐 / 6a9de89 devShell 自洽 / ea7284d 台账+跟踪框架）
- 工作区：干净
- 构建：✅ 容器内退出码 0；测试：✅ 50 passed / 0 failed（atcd-dev 容器 2026-09-11，R4 WS 桥合入后）
- 构建环境：atcd-dev 容器（root@10.0.100.244:/opt/atcd，`nix develop -c`，零环境变量）；
  工作站同（devShell 已自洽）；完整台账 docs/atcd-build-infra.md

## 待办顺序（下次继续从此处开始）

1. ~~R0-R4、R7、R7a、R8、R9、R10~~ 已完成（2026-09-11 @3f94e0d；分支全绿终审：
   fmt --check / clippy -D warnings / cargo test 50 passed 0 failed，atcd-dev
   容器跑通与 CI 完全一致的序列）
2. R10a（需真实账号登录，用户侧资源）：device 轮询 pending 语义实测
   （atcd 以 400 为 pending vs codex 源码 403/404——必有一方不符真实服务）
3. 金样本补齐：codex TUI 模式（交互式需人驱动）；omp 捕获（需改本机 omp
   配置，建议独立会话执行，避免扰动运行中的 harness）
4. ~~R3 剩余缺口：input items 内嵌身份、tools schema 深度差异~~ 已完成（2026-09-11 三金样本实证：input 内嵌身份仅 codex 客户端本地生成的 `msg_<uuid v7>` message id，透传安全；tools 形状为 codex 家族真子集、无上游 schema 校验拒绝形态。均无需改 atcd 行为，证据见 R3 行备注）
5. push / fork PR / atg-builder runner 注册：用户决策项（ci.yml self-hosted
   模板已留档 .scratch/atcd-ci-nix/spec.md Comments）
