# Plan 147 — 一道在第一个"不"上就停下的门

> 来源:2026-09-15,plan 146 收尾时报告 `verify.py --corpus-only` 是红的、且与改名无关。
> 用户先确认这条留作独立 plan,随后改口「解决报红」。

## 一、红的是什么

`refs/claude-code-2.1.220/verify.py --corpus-only` 是 CI 的最后一步。它在
`Plan 52 native run_agent/task graph/wait/stop surface drift` 上失败——而这条 `require`
是一条 39 分句的 `and` 链,报错信息只说"漂了",不说哪一句漂。

把 `and` 链拆开逐条求值(用 `ast` 取出那条 `require` 的每个 operand 单独 eval),红的是两句:

| 分句 | fixture 期望 | 实现实际 | 谁改的 |
|---|---|---|---|
| `depth_zero_native_tools` | 末尾有 `stop_program` | 没有 | **Plan 113**:`stop_program` 随 `run_program` 挂在 `Gate::Surface(Program)` 上,该 surface 默认关 |
| `agent_schema.properties` | `{…, max_rounds}` | `{…, model}` | **Plan 106/108** 把 `max_rounds` 移出 schema(上限固定在代码里,两个错答案都实测过);**Plan 92** 加入 `model` |

三处都是实现侧的**主动决定**,且 Rust 侧已把理由写在注释里
(`plan52_parity_tests.rs:486` 明写"`stop_program` left this list with plan 113";
`tools/mod.rs:2200` 明写轮次上限为什么不给模型)。**是 fixture 该追实现,不是反过来。**

## 二、fail-fast 掩盖了多少

`require` 一红就抛,所以 Plan 52 之后的检查从 2026-08-19(Plan 92,`2e70f22`)起
**一次都没跑到过**——将近四周。修掉 Plan 52 之后立刻露出第二条:

`Plan 56 parser contract drift` —— fixture 期望 `<REPO>/.claude/worktrees/…` 和分支
`worktree-serial`,实现是 `.kloop/worktrees/…` 和 `kloop-worktree-serial`。这正是
**2026-08-31 那次回退**(HANDOFF 的 plan 35 修正条):Plan 56 当初为 exact parity 抄了 cc
的字面目录名和分支前缀,后来判定"把自己的状态写进另一个产品的目录"不是 parity 的意思,
改回 kloop 自己的命名空间;`verify.py` 里这三处路径 + 一处分支名没跟着改。

**关键区分**:`fixtures/normalized/*.json` 和 `collect.py` 里的 `.claude/worktrees/`
**不能动**——那是 cc 2.1.220 的真实录制,cc 确实用那个目录。要改的只有 `verify.py` 里
针对 **kloop 自己**那份 `surface: "kloop-native"` 报告的断言。

修完这两条,后面的 Plan 57/58 native、scripted provider(7 测试)、sensitive information
一次通过——**它们本来就是好的,只是四周没人看见**。

## 三、做什么

`refs/claude-code-2.1.220/verify.py`,两个函数里六处期望值 + 两段注释:

- `_verify_plan52_native_report`:`depth_zero_native_tools` 去掉 `stop_program`;
  `agent_schema` 的 properties 集合把 `max_rounds` 换成 `model`。
- `_verify_plan56_native_report`:三处 `<REPO>/.claude/worktrees/` → `.kloop/`,
  一处 `"branch": "worktree-serial"` → `"kloop-worktree-serial"`。
- 两处各补一段注释,写清**为什么偏离 Plan 52/56 当初写下的值**(指向 plan 92/106/108/113
  与 2026-08-31 的回退),并点明 `fixtures/normalized/` 保留 cc 字面值是有意的。
  没有这段注释,下一个人对着 cc 的录制会把它"修"回去。

## 四、非目标

- **不动任何 `.rs`**。三条漂移全部是实现侧有据可查的主动决定,没有一条是 bug。
- **不动 `fixtures/normalized/` 与 `collect.py`**(cc 的录制)。
- **不改 fail-fast 本身**。把 `require` 改成"收集全部失败再报"是另一个设计决定
  (会改掉所有 negative-control 的语义),这次只让门重新通。教训见 HANDOFF 139。

## ✅ 已完成(2026-09-15;提交 SHA 以本条所在提交为准)

六处期望值与两段注释如上(plan 52 两处、plan 56 四处)。`verify.py --corpus-only` 现在
`Claude Code 2.1.220 parity baseline verified (corpus)`,退出码 0。

### 验证

`python3 -B refs/claude-code-2.1.220/verify.py --corpus-only` 退出码 0(CI 最后一步的
原样命令)。`cargo fmt --all --check`、`cargo clippy --workspace --all-targets
--all-features -D warnings`、`cargo test --workspace` 三条各自单独跑并当场取退出码,
全为 0(本次未改 `.rs`,照仓库完成标准跑)。

**过程中的一个自伤**:拆 `and` 链时我用 `python3` 直接 import 了 `verify.py`,没带 `-B`,
在 `refs/claude-code-2.1.220/` 下留了 `__pycache__`,于是 `verify_sensitive_information`
报 `Python cache artifacts present`。CI 的命令是 `python -B`,不会有这问题;清掉即可。
