# Plan 145 — 一个永远不会重复的键,不该留在磁盘上

> 来源:2026-09-15,plan 144 收尾时我问「逐字规则还留着还是退回会话级」,
> 用户先问「能判断是逐字规则吗」,确认能判之后拍板「同意」退回会话级。

## 一、为什么现在推翻 plan 135 第四节

plan 135 给 opaque 脚本加 project 档,是用户当场 dogfood 说「只是对话级,还是不方便」。
那句话成立的前提是**几乎每条测试命令都是 opaque** ——而那个前提正是 plan 144 修掉的
`2>&1`。修完之后:

- 还会落到逐字路径上的,只剩 `cat > probe.go <<'EOF'` 这类 heredoc 探针;
- 实测那份 `permissions.json`:38 条逐字规则,**互不相同,复用率 0**;
- 逐字规则的命中条件是整条命令原文全等,而真实命令里带着这一次特有的东西(测试名正则、
  包列表、`tail -3` 的 3)。

**能判断的是"这条是不是逐字规则"(`remember_payload` 的 opaque 分支是唯一产地,
`Rule::BashScript` 是唯一形态),判断不了的是"它以后还会不会命中"。** 既然判不了,就不该
用磁盘去赌:每按一次 `p` 落一条永不命中的规则,只会把真正有用的那几条埋掉。

## 二、做什么

- `remember_payload` 的 opaque 分支不再给 `rules`,于是 `persistable()` 为 false、
  `ApprovalScope::Project` 自然不上菜单;`signatures`(会话键,含 `!no-sandbox` 位)原样保留。
- 删 `Rule::BashScript`、`parse_rule` 的 `bash_script` / `bash_script_no_sandbox` 分支、
  `Rule::allows_script` / `Rule::hits_script`;`allow_rules_match` 与 `rules_hit` 的 opaque
  分支回到 `false` ——**没有任何规则能替一条没被解析过的脚本背书**,whole-tool `bash` 也不行。
- `OpaqueScript` 留着:它现在只为会话签名服务(原文 + 逃逸位)。

**磁盘核查(删解析分支的前提)**:`~/.kloop/projects/v1/` 下 7 个项目桶,`bash_script` 规则
0 条——plan 144 清掉的那份是唯一带过的。所以没有哪份 policy 会因为"读到一条不认识的规则"
而整份 fail-closed。

## 三、非目标

- **会话级记忆照旧**:`a` 仍然按命令原文记住,同一会话内重跑同一条不再问;逃逸位仍然编进
  签名(沙箱内的 yes 不覆盖脱沙箱运行)。
- **不动 PowerShell**(它连会话级都没有,理由见 plan 135)。
- **不动沙箱升级那道门**的会话通配(plan 129)。

## ✅ 已完成(2026-09-15;提交 SHA 以本条所在提交为准)

`crates/core/src/permissions.rs`:删 `Rule::BashScript`(连同三处 `match` arm)、`parse_rule`
的 `bash_script` / `bash_script_no_sandbox` 分支、`Rule::allows_script` / `Rule::hits_script`;
`allow_rules_match` 与 `rules_hit` 的 opaque 分支回到 `false`;`remember_payload` 的 opaque
分支 `rules` 置空(`persistable()` 因此为 false,project 档不上菜单),会话签名与
`!no-sandbox` 位原样。`VERBATIM_REMEMBER_RULE` 的文档改述("没有规则可回显"而不是"回显会重复
屏幕上的命令")。净删 60 行。

README 四处:规则语法表(不再列 `bash_script(...)`)、Asking 段(只剩 `a`,并写明为什么没有
`p`——38 条 0 命中)、测试索引的 "opaque never cacheable"(这句在 plan 135 之后就已经过期)。

### 测试

- `opaque_bash_is_remembered_verbatim_for_the_session` 改用**有可写 store** 的 gate:
  `approval_scopes` 仍然只有 `Once + WorkspaceSession`——有地方落盘却不给 project 档,这才
  是一句话而不是巧合;末尾断 writer **一次都没被调用**。
- `rule_parsing_accepts_valid_and_rejects_malformed` 加两条:`bash_script(...)` /
  `bash_script_no_sandbox(...)` 现在解析失败(旧版本写下的 store 也一样拦在门外)。
- 删掉 `a_verbatim_grant_persists_as_a_bash_script_rule` 与
  `a_bash_script_rule_knows_which_run_was_consented_to`(它们测的是被撤掉的那条路)。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、
`cargo test --workspace`,各自单独跑并当场取退出码。
