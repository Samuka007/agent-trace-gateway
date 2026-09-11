# codex fork (atcd-libs) 补丁台账

- 生成时间：2026-09-11T09:35:16Z（由 scripts/fork-sync 自动生成，勿手改；目的映射在脚本 purpose_for()）
- 补丁源：`git@github.com:Samuka007/codex.git` 分支 `atcd-libs` @ `9f95fe1808f1881451d9fc0b192011fd2f1fdda6`
- 重放基线：openai/codex `main` @ `02a8f038b87ad34d4a1dc5058eda26972ed7aa6c`
- 补丁集：`git rev-list --reverse upstream/main..origin/atcd-libs`（共 2 个）
- atcd 依赖：agent-trace-gateway Cargo.toml 的 codex-core/codex-protocol 指向本分支；
  用途 = 公开 CodexResponsesMetadata 字段可见性与模块路径（docs/atcd-requirements.md R1a/R8）。
- 重放/验证：`scripts/fork-sync`（am -3 重放 + cargo check -p codex-core -p codex-protocol）

## 补丁清单

### 1. `a77161ebe9` atcd: expose CodexResponsesMetadata projection publicly

```
Author: Samuka007 <atcd@local>
Date:   2026-09-11T05:39:15Z

atcd: expose CodexResponsesMetadata projection publicly

Fork-only patch for agent-trace-gateway: flip visibility in
responses_metadata.rs so external consumers can construct/serialize
turn metadata and client_metadata/compatibility-header projections
with by-construction fidelity instead of wire transcription.

```

```
 codex-rs/core/src/responses_metadata.rs | 188 ++++++++++++++++----------------
 1 file changed, 94 insertions(+), 94 deletions(-)
```

**目的**：把 codex-rs/core/src/responses_metadata.rs 内 94 处 pub(crate) 翻转为 pub：CodexResponsesMetadata 全部字段与 new()、client_metadata()/turn_metadata_json() 等投影方法、CompactionTurnMetadata、TurnMetadataWorkspace、请求种类枚举与 KEY 常量对 crate 外可见。atcd 侧用途：src/atcd/rewrite.rs 的 mint_metadata() 直接构造该结构体并逐字段赋值（turn_id/root_turn_id/context_window_id/agent_name/request_kind/sandbox/window_number 等），apply_third_party() 调 turn_metadata_json()/client_metadata() 生成 wire 投影——字段名/类型/序列化的正确性由 codex 自身代码保证（by construction），替代 wire 转录。对照 docs/atcd-requirements.md R1a（依赖对齐决策）与 Cargo.toml 中 codex-core/codex-protocol（git = Samuka007/codex, branch = atcd-libs）处的注释（R8 行）。

### 2. `9f95fe1808` atcd: pub mod responses_metadata

```
Author: Samuka007 <atcd@local>
Date:   2026-09-11T06:04:51Z

atcd: pub mod responses_metadata

```

```
 codex-rs/core/src/lib.rs | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)
```

**目的**：codex-rs/core/src/lib.rs 一行：mod responses_metadata → pub mod。使模块本身可被外部 crate 引用（否则上一补丁翻转的 pub 项仍困在私有模块内）。atcd 侧用途：rewrite.rs 的 use codex_core::responses_metadata::{CodexResponsesMetadata, CodexResponsesRequestKind}依赖该模块路径公开。与上一补丁构成完整的最小可见性补丁集。

