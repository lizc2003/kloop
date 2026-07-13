# Plan 23 — slash 命令(✅ 完成内置面;用户模板挂账)

## 完成记录(2026-07-13,提交待补)

开工时用户**转向 scope**:本 plan 只做**内置 slash 命令**(`/help` `/cost`
`/compact` `/clear`),用户自定义 `.kloop/commands/*.md` 模板整体挂账(留同一派发
层,以后独立 plan)。理由:kloop 现在完全没有 slash 基础设施(连手动压缩、看 token
账都没入口),而用户模板对早期单人工具是投机性的;派发层内置与将来模板共用,先做内置
不堵死模板。用户另定**代码结构走目录**(`core/src/commands/`,一命令一文件,目录清单
即命令目录)。

**做了什么**:
- `core/src/commands/`(目录模块):`mod.rs`(`SlashResult{output,cleared}` +
  `Builtin` 注册表 + `is_command` + `run` 派发 + 未知列清单)/ `help.rs` / `cost.rs`
  / `compact.rs` / `clear.rs`——**一命令一文件,文件名即命令**。`run(line, &mut
  History, &Arc<Config>, &cancel)` 纯函数式:各命令按需取 History/cfg,返回 output
  文本 + cleared 标志,不自己碰 UI(渲染是前端的事)。
- `/help` 列注册表(名字对齐);`/cost` = `model + ~used/window tokens (pct%)`(读
  `history.estimated_tokens()` + `cfg.context_window`,累计花费/价目挂账);`/compact`
  调 `compact::run_compaction`;`/clear` = `history.replace_all(vec![])`
  (compacted-empty 标记,resume 回放到空,cc/claw"清空本会话不 fork"语义)+ 清
  `cfg.todos`/`cfg.inbox`;未知 `/x` 列可用清单(同 agent_type 形态)。
- **重构** `compact::run_compaction`:去掉 `ui` 参数、返回 `CompactionStats{summarized,
  kept}`,note 上移到 agent.rs 两个调用点——`/compact` 的报告因此走 `SlashResult.output`
  一条路,不双份。
- 触发规则:输入以 `/` 起 + 后随非空白 = 命令行;**仅空闲时执行**(命令读写 History,
  turn 正用着),running 时 `/`-行当 steering 文本(Ctrl+C 仍是硬停)。plain REPL 内联
  执行(自己持 History);TUI 路由进 worker(持 History)——`WorkerMsg::{Turn,Command}`,
  慢的 `/compact` 显示 busy + Ctrl+C 可中断;结果经新 `AgentEvent::{System,ClearTranscript}`
  回流,`Cell::System` 多行 dim 渲染(不像 Note 单行截断),`/clear` 清转录区。
- 测试 +10:commands 单元(is_command 检测、help 列全、cost 对窗口百分比整串、cost 无窗
  口、compact 摘要+计数、clear 清 History+todos+inbox、未知列清单)、tui(idle slash →
  `Command::Slash`+busy 无 User cell / running 时 `/`-行转 Steer、System/ClearTranscript
  apply)、render(System 多行 dim)、compact stats 断言。331 全绿,fmt/clippy 净。

**验收**:plain REPL 真跑 `/help`(对齐清单)、`/cost`(model+上下文)、未知(列清
单)、`/clear`(已清)全过;`/compact` 触发模型,**真 key 验收挂账**(本 checkout 无
`.kloop/env.local`,需向用户要 key/代理)。TUI 路由靠单测锁定(无法管道验)。

**挂账/不做**:用户自定义 `.kloop/commands/*.md` 模板(`$ARGUMENTS`/`$N` 替换,回源见
下——只有 cc 有、0 索引、frontmatter;kloop 落地时接 `commands/custom.rs` 姊妹 + `run`
match 前加 lookup)、server-mode slash、`!bash`/`@file` 注入、命名空间/子目录。

---

# Plan 23 — 自定义 slash 命令(原备忘,已转向内置面)

> 备忘,未开工。开工前读 HANDOFF。参考:cc `.claude/commands/*.md`(可复用带参
> prompt 模板,`$ARGUMENTS`/`$1` 替换,`!bash`/`@file` 注入)。回源核对(教训 11)。

## 回源结论(三家真读,2026-07-13)

**只有 claude-code 有此功能** —— 无第二家独立收敛作交叉验证(不同于 fork/steering
两家收敛),形态基本只能抄 cc 一家,收敛信号弱。

1. **claude-code —— `.claude/commands/*.md`(唯一参考)**。发现层:项目
   `.claude/commands/` + 个人 `~/.claude/commands/` + 插件;子目录 = 命名空间(路径段
   用 `:` 拼,如 `frontend:component`)。文件 = md 正文(模板)+ **可选 YAML
   frontmatter**;命令名 = 文件基名去 `.md`。frontmatter 字段全可选:`description` /
   `argument-hint` / `arguments`(命名参数)/ `allowed-tools` / `model`(`inherit` =
   继承)/ `effort` / `disable-model-invocation` / `user-invocable` / `shell` / `name` /
   `version` / `when_to_use`。描述:frontmatter `description` 优先,否则
   `extractDescriptionFromMarkdown`(取首个非空行、剥 `#` 标题、>100 截断)。替换
   (`substituteArguments`,`src/utils/argumentSubstitution.ts`):`$ARGUMENTS`(整串原
   文)、`$ARGUMENTS[N]` 和 `$N`(**0 索引**,shell-quote 拆词)、命名参数 `$foo`(按
   位置映射);**正文无任何占位符且有参 → 追加 `\n\nARGUMENTS: {args}`**;参数用
   shell-quote 拆(引号成词),失败退化空白拆分。`!bash`/`@file` 注入是另一套。
2. **claw-code —— 只有内置 slash**(`/help`/`/clear`/`/compact`/`/session`/skills),
   `rusty-claude-cli/src/input.rs` 的 `SlashCommandHelper` 只做补全,无用户 `.md`
   模板;`ARGUMENTS` 命中都在 tools/plugins/权限代码,非命令模板。
3. **codex(本 checkout)—— 无此功能**。`codex-rs/prompts` 是内置系统提示模
   板;`tui/.../custom_prompt_view.rs` 只是 review 指令的文本输入弹层。上游 codex 有
   `~/.codex/prompts/*.md`(`$1`/`$ARGUMENTS`)但这份 fork 未暴露/已裁。

**两个教训 11 点**:①plan 备忘写 `$1 $2`(1 索引),cc 实际是 `$0`/`$ARGUMENTS[0]`
(0 索引)—— plan 是二手;②cc 用 frontmatter 存 description,但 kloop 可砍掉
frontmatter、描述取首行来躲 YAML 依赖(和 plan 17 片 2 agents 躲 YAML 同理由)。

## 目标

用户定义可复用、带参的 prompt 模板;输 `/name args` 展开成 prompt 喂给 agent。和刚
做的 `[agents.<name>]`(plan 17 片 2)是姊妹形态——命名定义 + 参数,面向 CLI 生产力。

## 关键决定(开工时定 / 问用户)

- **文件形态**(必问用户):`.kloop/commands/<name>.md`(md 正文 = 模板,可选
  frontmatter 放 description)还是 config.toml `[commands.<name>]`。张力:命令模板是长
  prose,md 更顺(和 AGENTS.md 同类);但 agents 选了 config.toml(A),不一致。
  plan 17 片 2 用户选 config.toml 的理由是"躲 YAML 依赖"——但命令模板正文不需要
  frontmatter 解析(纯 md body + 文件名即命令名),可以避开那个理由。开工时定。
- **参数替换**:`$ARGUMENTS`(整串,cc 主形态)+ `$1 $2`(位置)。倾向抄 cc。
- **触发**:TUI/plain 输入以 `/` 开头且匹配已定义命令 → 展开成 user 消息;未知 `/x`
  报错列可用清单(同 agent_type 形态)。与内置 REPL 命令(exit 等)不冲突——内置
  优先。
- **展开时机**:纯文本替换成 user 消息即可(最小版);cc 的 `!cmd` 注入 shell 输出、
  `@file` 注入文件内容挂账。
- **内置 slash 面**:顺带定不定 `/help`/`/cost`/`/compact` 这类内置命令?本 plan 先只
  做**用户自定义**,内置命令另议(与 usage/cost 呈现那条备选可合并)。

## 不做

`!bash`/`@file` 注入(先纯模板);命名空间/子目录;内置命令全集;命令里再调命令。

## 测试

模板解析(md body / frontmatter)、`$ARGUMENTS`+`$1` 替换、未知命令报错列表、与内置
REPL 命令不冲突、缺参数处理。

## 完成标准

fmt/clippy/test 绿;真 key 一次自定义命令展开跑通;README、HANDOFF。
