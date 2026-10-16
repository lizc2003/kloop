# Plan 186 ✅ — 管线才是本体,另外四样只是它的邻居

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**本批最大的三条之一,建议放最后**——
> 它的语义风险最高,前面九条做完之后手感最好。

> **2026-10-14 现状校准（仅修订，拆分仍待实施；修订提交以本条所在提交为准）**：
> bypass 已移到 deny、敏感读取硬拦截与 plan gate 之后，跳过后续工具审批。
> 当前层级按执行顺序编号；下文行号与规模是起草时的快照，实施前须重新核对。

## 一、现状（立项读数，开工复核见第五节）

`rust/crates/core/src/permissions.rs`,**1632 code 行 / 4177 总行**。
模块 doc 开头就写清了它的本体是一条管线:

> deny rules → sensitive-read hard block → plan-mode read-only gate → bypass → safety checks →
> ask rules → session scheduler controls → sandbox auto-allow →
> read-only self-verdict → contained writes → allow rules → session cache → ask the user

但文件里另外还住着四样:

| 行 | code 行 | 是什么 |
|---|---|---|
| 1–288 | 167 | 对外类型:`Decision` / `Mode` / `ConfirmRequest` / `Approver` / `PermissionRules` / `ProjectAllowRules` / `ProjectPolicySnapshot` / `ProjectPermissionWriter` |
| 289–459 | 140 | **规则语言**:`Rule`、`parse_rule` / `parse_rules` / `parse_prefix_pattern` / `argv_has_prefix` |
| 460–659 | 172 | **策略存储**:`ModeState` / `GlobalPermissionPolicy` / `ProjectPolicyState` / `ProjectPermissionPolicy` / `ProjectPolicyRegistry` / `WorkspacePermissionCache` / `PermissionSession` |
| 660–1406 | 524 | **管线本体**:`Permissions` 与它的 `impl`(**留下**) |
| 1407–1958 | 441 | **调用事实**:`Hazard` / `ShellFacts` / `OpaqueScript` / `PathFacts` / `CallFacts` + 敏感路径侦测那一整套(`path_is_sensitive` / `bash_reads_sensitive_path` / `powershell_mentions_sensitive_path` / `recursive_search_covers_sensitive_path` / spill 掩码 / `lexical_normalize` / `fs_fold`) |
| 1959–2240 | 188 | **记忆与文案**:`Remember` / `remember_payload` / `bash_prefix_memory` / `describe` / `describe_parts` / `call_detail` / `tool_title` |

第四块(调用事实)回答的是"**这次调用摸到了什么**"，不依赖 `Permissions`。
采集仍包含路径规范化、文件系统解析与内置工具只读判据调用；第五块除了 UI
字符串，还构造实际授权用的记忆载荷，两者均原样迁移。

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
  contained-writes 的 cwd 边界。重构提交的 diff 里,`impl Permissions` 那一整段
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
- `permissions.rs` 降到 ≈690。
- `impl Permissions` 的 diff 除 `use` 外为零。

## 五、✅ 完成（2026-10-16）

实现提交：本节所在提交（`refactor(plan186): split permission gate responsibilities`）。
提交号可用 `git log -1 --format=%h --grep='^refactor(plan186):'` 查询。

开工复核为 **1674 生产 code 行 / 4409 总行 / 61 条测试**，第一节的规模与行段
为立项快照。沿用 plan 181–185 的物理行口径：取底部 `#[cfg(test)] mod tests`
之前的非空、非 `//` 注释行，不展开条件编译。

| 文件 | 职责 | 生产 code 行 / 总行 | 原测试分布 |
|---|---|---|---|
| `permissions.rs` | 公开类型、门面与完整管线、审批/升级/模式切换 | 703 / 2842 | 53 |
| `permissions/rule.rs` | 规则语言、解析与 allow/deny/ask 两个方向的匹配 | 163 / 264 | 3 |
| `permissions/policy.rs` | 全局约束、项目策略与工作区会话状态、epoch 发布 | 199 / 300 | 1 |
| `permissions/facts.rs` | shell/路径事实、敏感读取、危险特征与路径豁免 | 457 / 638 | 3 |
| `permissions/describe.rs` | 审批记忆载荷、拒绝说明与展示文案 | 224 / 419 | 1 |
| `permissions/testutil.rs` | 策略与管线共用的项目存储夹具及测试 cwd | 测试专用 / 70 | — |

- `impl Permissions` 整段逐字节一致，原有 13 层顺序、bypass、拒绝/停止、升级
  授权来源、工作区隔离、持久化先于内存发布与 epoch 更新保持原实现。
- `allow_rules_match` / `rules_hit` 随规则语言迁移；项目策略继续调用前者，
  规则匹配继续读取 `CallFacts` / `PathFacts`。事实模块不依赖 `Permissions`，
  也不把既有文件系统解析或内置工具只读判据换成新接口。
- 原有 20 个公开符号与 4 个 crate 内符号保留原路径及可见性。四个策略类型、
  `CallFacts` 和 `user_stop` 从父模块重导出；子模块均私有，跨模块内部帮助项和
  实际读取的字段仅提至 `pub(super)`，同模块的规则匹配帮助方法继续私有。
- 词法核对 175 个函数的签名与正文、43 个类型/常量/trait：除可见性与 rustfmt
  格式外均一致。61 条原测试名称、属性和正文均保留；直接测规则、事实、策略
  刷新与记忆载荷的 8 条随模块走，经过完整权限门的 53 条留在父模块。
- `doc_placement` 的敏感路径文档检查仅修改源文件路径，两个函数的文档正文
  与附着关系不变；迁移后的另两处 rustdoc 链接改为父模块限定路径，避免为
  文档单独导入类型引发 unused-import 告警。DESIGN 原位补齐职责边界与目录表。

Windows 在独立临时 workspace/target 中钉住 `1.98.1`，检查真实 core 的
`--target x86_64-pc-windows-msvc --all-targets --all-features`。四个职责模块、
四处迁移单测与共用夹具的九处故意错误均报预期 E0425；恢复全部 crate 输入并
逐字节核对、清理 core 产物后通过，仅有原先 `TestConfig::login_env` 的
dead_code 告警。按教训 201，仅 Windows triple 的 C 编译/归档生成空产物，
不对 Rust 接口做 stub；本检查不代表 C 代码、链接或 Windows 执行通过。

最终源码在外层执行沙箱之外的完整 `make check` 全绿：fmt、clippy、workspace
debug 全量测试与本机 parity 均通过。61 条权限测试、4 条文档附着检查、core
1124 条、CLI 154 条、plain PTY 7 项、TUI PTY 20 项、server 26 条单测与
40 条协议集成测试全部通过；真实 API 与诊断测试按原设置忽略。首轮 clippy 的
两项文档专用导入告警已修正，未放宽门禁。完整差异、`git diff --check` 与
`git diff --cached --check` 通过；教训见 HANDOFF 240。

本机语料中七条 kloop 证据的源码位置已另备修订候选，尚未应用，待按 `refs/*`
只读约定单独批准。当前 parity 只验证这些位置仍在合法范围内，不证明旧行段仍
对应其声明；该本机证据维护事项不进本次提交。
