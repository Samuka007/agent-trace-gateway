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
| R3 | 其他 responses 原生 agent（omp/opencode）支持：无 codex 身份头时铸造 v7 身份树 + 合成 turn 元数据（17 字段全集），body 信封合成（store/include/prompt_cache_key=铸造 session/client_metadata/键序对齐金样本），会话键 prompt_cache_key → body 前缀 sha256 兜底；instructions/工具表保留其自洽家族 | 用户需求（已确认 omp 支持 Responses） | 已完成（R3a 修法 A 已实现：cache key=铸造 session） | 冒烟路径B：body 信封全字段落位；opencode 金样本（R7a）实证：无 codex 身份头（铸造路径必要），prompt_cache_key=ses_ 会话 id 且两轮稳定，样本 scripts/fixtures/opencode_1.18.29.{headers.json,body.json}+_turn2.body.json |
| R3a | 【R3 开放问题→已决策】cache key 与 session-id 一致性：采用修法 A（prompt_cache_key 重写为铸造 session_id，对齐"cache key 派生自 session"的真实不变量），信封合成实现 | 自查发现 | 已完成 | rewrite.rs codex_envelope_body + 单测；冒烟路径B |
| R4 | WebSocket V2 支持 | 用户需求 | 待办（已定位协议源码） | codex 侧协议在 codex-rs/codex-api/src/endpoint/realtime_websocket/（protocol_v2.rs / methods_v2.rs）；下一步读 wire 出设计 |
| R7 | 真实下游流量捕获（金样本）：codex 0.153.4 custom provider (wire_api=responses) 模式实测，nix 提供二进制 | 用户审计提问（"你真的捕获过下游流量吗"） | 已完成（首个样本） | scripts/fixtures/codex_exec_0.153.4.{headers.json,body.json}；发现：①身份头全套存在（session-id/thread-id/window/turn/β-features），codex_native 判定成立 ②session_id==thread_id（exec 模式）③turn 元数据 17 字段（agent_name/context_window_id/request_kind/sandbox 等远超早期假设）④originator 随表面变化（exec=codex_exec），UA 带 "(codex_exec; 0.153.4)" 后缀 ⑤os_info 渲染第三次验证（NixOS 26.11.0）⑥流断自动重连 5 次 ⑦已据此升级 mint_turn_metadata 全字段集 + session==thread 对齐 |
| R7a | 金样本扩容（opencode）：responses wire 实测捕获（nix opencode 1.18.29；capture_opencode.sh 单轮实测跑通，fixtures 取 /tmp 改造版两轮驱动的同会话配对样本），对照 codex 金样本出方言差异清单 | 用户需求（台账待办 5） | 已完成 | scripts/fixtures/opencode_1.18.29.{headers.json,body.json} + opencode_1.18.29_turn2.body.json（turn1/turn2 同会话，prompt_cache_key 相等且==x-session-id）；方言差异 vs codex：①headers 无任何 codex 身份头（无 originator/session-id/thread-id/x-codex-*），代之 Bun 侧 x-session-affinity/x-session-id（=ses_ 会话 id）+ UA "opencode/1.18.29 ai-sdk/provider-utils/4.0.38 runtime/bun/1.3.13"，Accept:*/*（codex 为 text/event-stream）→ codex_native 判非 native 成立，铸造路径适用 ②body 无顶层 instructions（系统提示在 input[0] role=developer）、无 client_metadata；新增 max_output_tokens:32000；reasoning 多 summary:"auto"；store:false/include/stream/tool_choice/text.verbosity 与 codex 同值 ③prompt_cache_key=opencode 会话 id（ses_…），两轮请求完全一致（turn2 实证连续性）；title 副请求不带 prompt_cache_key ④input 内容为 parts 数组（input_text/output_text），assistant 历史（"ok"）客户端全量重发，无 previous_response_id——与 codex store:false 同构 ⑤每条用户消息附带一条 title-generator 副请求（独立 developer 提示、无 tools/prompt_cache_key/reasoning/text 字段），网关需容忍 ⑥工具表 9 个自洽家族（apply_patch/bash/glob/grep/read/skill/task/todowrite/webfetch），证实 R3"保留自洽家族"决策 |
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
- 构建：✅ 容器内退出码 0（产物 76MB）；测试：✅ 45 passed / 0 failed（atcd-dev 容器 2026-09-11）
- 构建环境：atcd-dev 容器（root@10.0.100.244:/opt/atcd，`nix develop -c`，零环境变量）；
  工作站同（devShell 已自洽）；完整台账 docs/atcd-build-infra.md

## 待办顺序（下次继续从此处开始）

1. ~~R0-R3、R7~~ 已完成
2. R4：WS V2 wire 设计 → 透传桥实现（进行中，.scratch/atcd-r4-ws-v2/）
3. R8：CI nix 化 + runner 利用评估（进行中，.scratch/atcd-ci-nix/）
4. R9：fork patch list 自动同步工具（进行中，.scratch/atcd-fork-patchlist/）
5. 金样本扩容：codex TUI 模式（余）；~~opencode（实测跑通）、多轮对话样本~~ 已完成 → R7a（opencode_1.18.29 单轮 + turn2 + title 副请求）
6. R3 剩余缺口：input items 内嵌身份、tools schema 深度差异——opencode 侧已实证（R7a：无内嵌身份项、tools 家族自洽）；omp 侧待真实流量捕获
7. R10：OAuth web/PKCE 远端登录流研究
