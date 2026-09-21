# Plan 186 — 管线才是本体,另外四样只是它的邻居

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**本批最大的三条之一,建议放最后**——
> 它的语义风险最高,前面九条做完之后手感最好。

## 一、现状

`rust/crates/core/src/permissions.rs`,**1632 code 行 / 4177 总行**。
模块 doc 开头就写清了它的本体是一条管线:

> deny rules → sensitive-read hard block → plan-mode read-only gate → safety checks →
> ask rules → session scheduler controls → sandbox auto-allow → bypass →
> read-only self-verdict → acceptEdits → allow rules → session cache → ask the user

但文件里另外还住着四样:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–288 | 167 | 对外类型:`Decision` / `Mode` / `ConfirmRequest` / `Approver` / `PermissionRules` / `ProjectAllowRules` / `ProjectPolicySnapshot` / `ProjectPermissionWriter` |
| 289–459 | 140 | **规则语言**:`Rule`、`parse_rule` / `parse_rules` / `parse_prefix_pattern` / `argv_has_prefix` |
| 460–659 | 172 | **策略存储**:`ModeState` / `GlobalPermissionPolicy` / `ProjectPolicyState` / `ProjectPermissionPolicy` / `ProjectPolicyRegistry` / `WorkspacePermissionCache` / `PermissionSession` |
| 660–1406 | 524 | **管线本体**:`Permissions` 与它的 `impl`(**留下**) |
| 1407–1958 | 441 | **调用事实**:`Hazard` / `ShellFacts` / `OpaqueScript` / `PathFacts` / `CallFacts` + 敏感路径侦测那一整套(`path_is_sensitive` / `bash_reads_sensitive_path` / `powershell_mentions_sensitive_path` / `recursive_search_covers_sensitive_path` / spill 掩码 / `lexical_normalize` / `fs_fold`) |
| 1959–2240 | 188 | **记忆与文案**:`Remember` / `remember_payload` / `bash_prefix_memory` / `describe` / `describe_parts` / `call_detail` / `tool_title` |

第四块(调用事实)最值得独立:它回答的是"**这次调用摸到了什么**",纯粹、可单测、
和"该不该放行"无关。第五块是给 UI 看的字符串,更不该和判定混住。

## 二、切法

| 新文件 | 内容 | 预估 |
|---|---|---|
| `permissions/rule.rs` | 289–459 整段 | ≈140 |
| `permissions/policy.rs` | 460–659 整段 | ≈172 |
| `permissions/facts.rs` | 1407–1958 整段 | ≈441 |
| `permissions/describe.rs` | 1959–2240 整段 | ≈188 |
| `permissions.rs`(留) | 对外类型 + `Permissions` 管线 + `pub use` | ≈690 |

## 三、坑

- **管线一行都不许动。** 这个文件是安全边界里最重的一个:deny 压过 allow 与 bypass、
  敏感路径永不进缓存、opaque 脚本只按会话记不落盘(plan 137/145)、
  `acceptEdits` 的 cwd 边界。重构提交的 diff 里,`impl Permissions` 那 524 行
  **除了 `use` 之外应该是零改动**——这是自查标准。
- **`CallFacts` 与 `Permissions` 之间的方向要保持单向**:facts 不认识 Permissions。
  现在 `impl CallFacts`(1458)里有没有反向依赖,开工第一件事就是确认。
- **`path_is_sensitive` 与 `path_tail_is_spill` 各自带着自己的 doc**,
  `cli/tests/doc_placement.rs` 有一条测试按名字钉住了这两段文档的位置。
  搬走它们会让那条测试红——**那条测试要跟着改路径,不是改文案**。
- `Rule` 是 `enum` 且 `parse_rule` 的错误信息是用户可见的(配置里写错一条规则就报它)。
  文案原样搬。
- 4177 总行里 2545 行是测试。**这是本批测试搬运量最大的一条。**

## 四、验收

- `make check` 全绿;permissions 的 2545 行测试一条不少。
- `cli/tests/doc_placement.rs` 的 `the_sensitive_path_list_keeps_its_own_documentation` 改到新路径后通过。
- 四个新文件 ≤800;`permissions.rs` 降到 ≈690 后跑 `make arch-baseline`,它会从基线里被摘掉。
- `impl Permissions` 的 diff 除 `use` 外为零。
