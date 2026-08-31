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
- 新增 `RETIRED_FILES`,把退休名与接替名成对登记,并在每个目录上跑一次
  `warn_retired`。

`README.md` 的 Instruction files 段落同步。仓库自身迁移:`CLAUDE.md` →
`AGENTS.md`(`git mv`,内容不变),`CLAUDE.md` 改成一行 `@./AGENTS.md`。

## 为什么必须带迁移提示

删掉一个回退名的失效形状是**静默**的:只有 `CLAUDE.md` 的仓库从"加载全部规则"
变成"加载零条规则",没有错误、没有退出码,agent 只是开始按不同的规则干活。而
这个症状离原因极远——Plan 106 整整查了一轮才把「codex 快是因为它什么指令都没
读到」认出来,那正是本次要主动制造的状态。

所以 `warn_retired` 的条件是「退休名存在 **且** 接替名不存在」:

- 两名并存 → 静默。这是**故意的** cc/kloop 分工(比如 `~/work/claude-code` 就
  两个都有,内容不同),每次都报会让提示变成噪音,噪音等于没有提示。
- 只有退休名 → 一条提示,同时给出两条出路(改名,或保留双份并在 `AGENTS.md`
  里写 `@./CLAUDE.md`),并印出**绝对路径**——找不到该改哪个文件的话,提示等于没给。

## 验证 ✅

单元测试(`context::tests`,18 条全绿),其中三条是本片新增/改写的:

- `claude_md_beside_agents_md_is_read_by_neither_name_nor_warning` — 两名并存时
  只读 `AGENTS.md`,**且断言 warnings 为空**(否则提示会在每个仓库上刷屏)。
- `claude_md_alone_loads_nothing_and_says_why` — 只有 `CLAUDE.md` 时加载 0 个
  文件、恰好 1 条提示,提示里同时含两个名字和绝对路径。
- `claude_local_md_alone_is_reported_the_same_way` — local 层同样处理。

端到端(真实二进制,免费:空 HOME + 无效 provider,打完提示即失败退出):

- 本仓库(两名并存):往 `AGENTS.md` 塞一个不存在的 `@` 导入,启动打出
  `instruction import not found` → 证明 `AGENTS.md` 确实被读;同时**没有**
  "no longer read" 提示 → 证明静默分支正确。验完删除探针行。
- 临时 git 仓库(只有 `CLAUDE.md`):打出完整提示含两个名字与绝对路径 →
  证明探针有能力产出阳性(教训 92)。

## 非目标

- 不动 `.kloop/rules/*.md`、`@` 导入语法、32 KiB 预算、层次顺序。
- 不动 `${CLAUDE_SKILL_DIR}` / `${CLAUDE_SESSION_ID}` 等 skill 环境变量——那是
  cc 的 skill 兼容契约,与指令文件发现无关。
- 不做自动迁移(不替用户改名或写文件):指令文件是用户的内容,静默重写它比静默
  不读它更糟。
