# Plan 23 — 自定义 slash 命令(备忘)

> 备忘,未开工。开工前读 HANDOFF。参考:cc `.claude/commands/*.md`(可复用带参
> prompt 模板,`$ARGUMENTS`/`$1` 替换,`!bash`/`@file` 注入)。回源核对(教训 11)。

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
