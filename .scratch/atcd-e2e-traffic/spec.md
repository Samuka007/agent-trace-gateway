# Spec: R11 — E2E 真实 agent 流量 + 观测分析

Status: in-progress
Owner: PM 验收；E2E-Traffic agent 执行

## Requirement

用真实 agent 作为下游、fake 上游（mock SSE）作为上游，端到端打通 atcd，双端捕获
wire 并产出流量分析报告：atcd 到底改写了什么、保真了什么、观测面看到什么。
上游 fake 即可（不消耗真实账号；login 已按用户指示封存）。

## Specification（含依据）

- 矩阵：下游 ∈ {codex exec, opencode, omp} × 上游 = mock_upstream.py（fake SSE）。
  三种 agent 的原生金样本已备（scripts/fixtures/ codex_exec_0.153.4 / 
  opencode_1.18.29 / omp_18.1.16），可直接做"裸 wire vs 经 atcd 后 wire"对照。
- 捕获点（双端）：
  1. downstream：agent → atcd 的请求（atcd 监听口的入站 wire）
  2. upstream：atcd → mock 的请求（mock 落盘的 headers/body）
  差异 = atcd 的改写足迹（预期：codex 路径身份头 installation 替换；第三方路径
  铸造身份树 + 信封合成 + prompt_cache_key 重写为铸造 session）。
- 分析维度（报告必答）：
  1. 每 agent：header 级改写清单（加了/删了/换了什么）与 body 级 diff
     （内容平面 instructions/input/tools 是否逐字节保真）
  2. prompt_cache_key 三态对照：裸 wire 值 → atcd 重写值 → mock 所见值
  3. atcd 侧观测：RUST_LOG 日志关键行、绑定表（store）写入（session→account）
  4. mock 的 SSE 响应能否被各 agent 正常消费（下行链路可用性）
- 二进制：容器 /opt/atcd/target/debug/atcd（终审门已验证的产物）或工作站
  target/debug（若存在且为最近构建）；agent 可容器内重建（rsync + nix develop）。

## Acceptance

- [ ] 三 agent × ≥1 轮全部经过 atcd 打到 mock（codex/opencode/omp 各有 downstream+upstream 成对捕获）
- [ ] 差异报告 report.md：三 agent 的改写清单 + 内容平面保真结论（逐字节比对结论）
- [ ] prompt_cache_key 三态对照表（裸 → 重写 → 上游所见）
- [ ] 绑定表/日志观测证据落盘
- [ ] 下行可用性结论：mock SSE 响应各 agent 是否消费成功（codex/opencode/omp 各一行）
- [ ] 发现 bug 只上报不改源码
