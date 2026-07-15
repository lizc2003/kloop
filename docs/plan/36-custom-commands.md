# Plan 36 — 用户自定义 slash 命令(plan 23 挂账领编号)

> 一句话定位:plan 23 挂账的"用户自定义 `.kloop/commands/*.md` 带参模板"正式领编号。
> 但生态位已变:plan 28 的 skills 已覆盖"用户自定义带参 prompt 包"(`/name` 触发 +
> `$ARGUMENTS`/`$N` 展开 + frontmatter),且 **cc 自己已把 commands 折进 skills 机制**
> ——本 plan 开工前要先审"独立 commands 系统"是否还成立,剩余 delta 可能只是一个兼容
> 发现目录 + 两种注入。

## 回源结论(2026-07-15 增量,推翻 plan 23 时代的旧图;file:line)

- **cc 现行:commands 已并入 skills**。用户/项目级 `.claude/commands/*.md` 经
  `skills/loadSkillsDir.ts` 加载,标 `loadedFrom:'commands_DEPRECATED'`——不是独立系
  统;frontmatter 字段与 skills 同集(description/argument-hint/allowed-tools/model/
  `context:fork`/…,`loadSkillsDir.ts:237-264`),参数替换同一个
  `substituteArguments`(`$ARGUMENTS`/`$N` **0 索引**/命名参数 `$foo`,
  `argumentSubstitution.ts:111-141`)。**kloop 的 skills `expand_body` 已实现同一形态**
  (plan 28)。plan 23 回源结论里"commands 是独立机制"的部分已过时(教训 14 的方向导
  数:cc 正把两者收敛成一个)。
- **cc 独有的剩余 delta(kloop 没有的)**:① 内嵌 bash 注入——代码块 ```` ```!cmd ````
  与内联 `` !`cmd` ``(`promptShellExecution.ts:49/56`),**经 BashTool + 权限校验执行**
  (:98-113),命令的 allowed-tools 注进本 turn 权限上下文;② `@file` 附件(提交时
  `processSlashCommand.tsx:1172-1185` 解析 @-mention);③ 子目录命名空间
  `namespace:command`(`loadSkillsDir.ts:523-534`);④ 优先级 managed > user >
  project、inode 去重(`markdownConfigLoader.ts:377-407`)。
- **codex/claw**:无用户命令模板(plan 23 已回源,本 checkout 无变化)。仍是"一家
  形态",但"commands=skills 别名"这个收敛方向让 kloop 的落法变便宜了。

## kloop 现状与落点

- skills(plan 28)已有:`.kloop/skills/<name>/SKILL.md` 发现、`/name args` 触发、
  `$ARGUMENTS`/`$N`/`${CLAUDE_SKILL_DIR}` 展开、frontmatter(description/context/
  model/allowed-tools)、模型自选 + catalog 渐进披露。slash 派发层(`commands::run`
  → skill lookup)就是 plan 23 说的"同一派发层"。
- 落点(待开工拍板,倾向):**不建独立 commands 系统**,而是——
  1. `.kloop/commands/*.md` 作为 skills 的**第二发现根**:单文件即命令(无目录、
     frontmatter 可省,description 取正文首行),复用 skills 全部展开/触发机制;与
     `.claude/commands` 下载来的文件格式兼容(命名映射照 plan 28 片 3 的开放映射)。
  2. `!cmd` 注入:展开时执行、输出内联进 prompt,**必须走 bash 权限门**(教训 19b:
     这是安全门,不因"用户自己写的模板"豁免;deny/沙箱照常)。
  3. `@file` 注入:展开时读文件内容附进消息(走 read 权限/敏感路径判定,plan 31 的
     `read_path_blocked` 复用)。
- 命令与 skill 撞名:项目 skills 优先(目录形态更完整),开工定。

## 关键决定(开工时定 / 问用户)

1. **形态总闸(必问)**:方案 A =「commands 目录作 skills 第二发现根」(倾向,顺 cc
   收敛方向,机制零新增);方案 B = 独立 `commands/custom.rs` 系统(plan 23 旧案,现
   在看是重复建设)。
2. **`!cmd`/`@file` 进不进首片**:纯发现根已闭环可用;两种注入各带权限面,可切片。
3. **模型自选**:commands 目录来的条目进不进 catalog(cc 的 `disable-model-invocation`
   语义)——纯用户快捷入口倾向**不进** catalog(只 `/name` 可调),与 skills 的模型自
   选区分开。
4. **命名空间**:子目录 `:` 拼接,倾向挂账(单人工具先平铺)。

## 不做(挂账)

命名空间/子目录;managed/policy 层;插件系统;`argument-hint`/命名参数 `$foo`(skills
侧一并挂账);命令里再调命令;`!cmd` 的输出截断/超时精调(首片抄 bash 工具现值)。

## 测试

发现:`.kloop/commands/foo.md` → `/foo` 可调、描述取首行、frontmatter 可省;与 skills
撞名优先级;展开:`$ARGUMENTS`/`$N` 复用断言;`!cmd` 走权限门(deny 的命令在模板里同
样被拒、输出内联);`@file` 尊重 `read_path_blocked`(敏感/deny 文件拒注入);catalog
不含 commands 条目(决定 3 取倾向时)。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 补 commands 目录;本文件补完成记录;HANDOFF
补能力条目;plan 23/28 的挂账清单同步销账。真 key 验一次 `/name args` 展开 + `!cmd`
注入闭环。
