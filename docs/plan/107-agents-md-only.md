# Plan 107 — 指令文件只认 AGENTS.md,CLAUDE.md 退休

## 背景

Plan 106 定位「审查任务慢 8 倍」时发现的直接诱因:同一个仓库里 kloop 读到了
`CLAUDE.md`、codex 一条指令都没读到(它只找 `AGENTS.md`,零命中)。两个 agent
在同一棵代码树上按**不同的规则**工作,而且没有任何提示说明这一点。

根因不是哪个名字对,是**一个仓库有两个可能的真值源**。统一成一个名字,顺带让
cc 通过 `@./AGENTS.md` 从同一份文件取内容。

## 改动 ✅

`crates/cli/src/context.rs`:

- `INSTRUCTION_FILE_NAMES: [&str; 2]` → `INSTRUCTION_FILE_NAME: &str = "AGENTS.md"`,
  `LOCAL_INSTRUCTION_FILE_NAMES` 同理只留 `AGENTS.local.md`。两个 `add_*_file`
  里的"先试 A 再试 B"循环随之消失。

**不留兼容层**(用户拍板:「不用考虑兼容性,我要干净的代码」)。第一版曾加过
`RETIRED_FILES` + `warn_retired`,在只有 `CLAUDE.md`、没有 `AGENTS.md` 的目录上
打一条迁移提示;连同它的三条测试一并删除。理由成立:kloop 没有装机量,那层代码
服务的是一个不存在的存量用户群,而唯一真实的读者(cc)由用户仓库里一行
`@./AGENTS.md` 解决,不需要 kloop 的加载器认第二个名字。同时删掉了
`only_agents_local_is_read_when_both_local_names_exist`——回退概念消失后,它测的
是一条不存在的代码路径。

`README.md` 的 Instruction files 段落同步。仓库自身迁移:`CLAUDE.md` →
`AGENTS.md`(`git mv`,内容不变),`CLAUDE.md` 改成一行 `@./AGENTS.md`。

## 验证 ✅

单元测试 `context::tests` 14 条全绿。本片不新增测试:常量收成单个名字之后,
"读别的名字"已经不是一条可达路径,为它写断言等于测试一个不存在的分支。

端到端(真实二进制,免费:空 HOME + 无效 provider,打完启动提示即失败退出):
往 `AGENTS.md` 塞一个不存在的 `@` 导入,启动打出 `instruction import not found`
并带绝对路径 → 证明 `AGENTS.md` 确实被读。验完删除探针行。这条探针有正反两面
的能力(改名后不报,改回来就报),不是无效探针;`--mock` 走 `context::mock()`
完全不读指令文件,拿它做这个探针才是无效的(教训 92 同型)。

## 非目标

- 不动 `.kloop/rules/*.md`、`@` 导入语法、32 KiB 预算、层次顺序。
- 不动 `${CLAUDE_SKILL_DIR}` / `${CLAUDE_SESSION_ID}` 等 skill 环境变量——那是
  cc 的 skill 兼容契约,与指令文件发现无关。
- 不做自动迁移(不替用户改名或写文件):指令文件是用户的内容,静默重写它比静默
  不读它更糟。
