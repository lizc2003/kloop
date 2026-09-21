# Plan 36 — 用户自定义 slash 命令(plan 23 挂账领编号)✅

> ✅ **三片全完成**。首片(纯发现根,提交 56f3e51):`.kloop/commands/*.md` 单文件作
> `SkillSource::Command` 进同一 skill 注册表,复用 skills 全部展开/触发/slash 机制。
> 二片(`!cmd`/`@file` 注入,提交 93570b1):`/name` 展开时执行内嵌 bash(走 bash 权限
> 门)+ `@file` 读文件附进 prompt(走 `read_path_blocked`)。三片(注入统一到 `skill`
> 工具路径,提交 2446ee5):模型激活的 skill 也展开注入,两条触发路径行为一致。决定与记录见文末。

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

## 完成记录(首片:纯发现根,提交 56f3e51)

**决定(开工时定,用户"同意"倾向)**:① 形态总闸取**方案 A**(commands 目录作 skills 第
二发现根,机制零新增);② 首片只做**纯发现根**,`!cmd`/`@file` 注入切到下一片(各带权
限面);③ commands **不进模型 catalog / `skill` 工具**(只 `/name` 可调,cc 的
`disable-model-invocation` 默认);④ 命名空间/子目录挂账(单人工具先平铺)。

**回源核对(2026-07-16,复核 plan 备忘无误)**:cc 源码参考
`src/skills/loadSkillsDir.ts` — `loadSkillsFromCommandsDir` 确支持单文件 `*.md`(+目录
SKILL.md),`loadedFrom:'commands_DEPRECATED'`,命令名=去 `.md` 文件名(frontmatter
`name` 被 `displayName:undefined` 覆盖),默认 `userInvocable:true`;无 `description`
时 `extractDescriptionFromMarkdown`(`src/utils/markdownConfigLoader.ts:52`)取首个非
空行、`^#+\s+` 剥标题、截 100 字("..." 收尾)、空则 "Custom command"。kloop 照此
1:1 落。codex/claw 无用户命令模板(本 checkout 无变化)。

**改动(纯逻辑在 core、IO 在 cli,照 skills 分层)**:
- `core/src/skills.rs`:`Skill` 加 `source: SkillSource{Skill,Command}`(Default=Skill,
  旧构造零改动);`Frontmatter` 加 `Default`(无 frontmatter 即默认);抽
  `Frontmatter::into_skill` 共享 context/model/allowed-tools 装配;新
  `Skill::parse_command`(name=文件名、description 可省取首行、source=Command);新
  `description_from_body`(cc `extractDescriptionFromMarkdown` 移植);`skills_catalog`
  过滤掉 Command;`lookup` 改吃 `impl Iterator+Clone` 让调用方定作用域(slash=全部,
  `skill` 工具=仅 model-invocable)。
- `core/src/tools/skill.rs`:`skill` 工具 lookup 只搜 `SkillSource::Skill`(模型猜中命令
  名也够不着)。
- `core/src/agent.rs`:`skill` 工具注册条件从"有 skills"改成"有 model-invocable skill"
  (只装了命令不白挂工具)。
- `core/src/commands/mod.rs`:slash fall-through lookup 传 `cfg.skills.iter()`(搜全部);
  模块 doc 更新(不再是"future plan / custom.rs")。
- `cli/src/startup.rs`:`load_skills` 顺带 `commands_from_roots`(项目 `.kloop/commands/`
  → 全局 `~/.kloop/commands/`,顶层 `*.md`、稳定排序、malformed 告警跳过)+
  `merge_commands`(撞名 skill 优先、命令补空位)。装配线其余一字不改(commands 就是
  Command 源的 Skill,随 `Config.skills` 流进所有 thread/子 agent)。

**测试(全绿)**:skills.rs — `parse_command` 文件名/source、首行取描述+标题剥离+截断、
catalog 排除 Command、`lookup` 作用域(命令对模型不可见);commands/mod.rs — Command 经
slash 展开成 turn + 进 unknown 列表;startup.rs — `commands_from_roots` 发现/首行描述/
项目优先/非 md 与子目录忽略/malformed 告警、`merge_commands` skill 撞名优先。

**真 key 验收(anthropic 轨,scratch cwd 跑 `--plain`)**:`.kloop/commands/greet.md`(带
description + `$0`)→ `/greet Ada` → 模型回 `kloop-cmd-ok greeting Ada`;
`.kloop/commands/plain.md`(**无 frontmatter**,首行取描述 + `$ARGUMENTS`)→
`/plain hi there` → 模型回 `plain-ok hi there`。发现 + frontmatter 可省 + 首行描述 +
`$0`/`$ARGUMENTS` 展开 + slash→turn 全闭环。(`--mock` 跳过发现,验不了,故用真 key。)

## 完成记录(二片:`!cmd`/`@file` 注入,提交 93570b1)

**回源核对(2026-07-16)**:cc 源码参考——`!cmd` 在
`src/utils/promptShellExecution.ts`:`BLOCK_PATTERN=/```!\s*\n?([\s\S]*?)\n?```/g` +
`INLINE_PATTERN=/(?<=^|\s)!`([^`]+)`/gm`(inline 要求 `!` 前是行首/空白),在命令
`call()` 里**参数替换、`${CLAUDE_SKILL_DIR}` 替换之后**执行;先 `hasPermissionsToUseTool(
BashTool)` 校验、非 allow 抛 `MalformedCommandError` 中止,再 `shellTool.call` 跑、输出
`String.replace` 内联(函数 replacer 防 `$&` 破坏);命令 frontmatter `allowed-tools` 注进
本 turn 权限上下文的 `alwaysAllowRules.command`;**MCP skill 永不执行**内嵌 bash。`@file`
在 `processSlashCommand.tsx`:展开正文文本经 `getAttachmentMessages` 解析 @-mention,读文件
内容附成**独立消息**(与普通 @-mention 同机制)。

**落点(kloop,只在 `/name` 用户路径,`skill` 工具模型路径挂账)**:注入是有副作用的展开,
不能进纯函数 `expand_body`,新 `core/src/tools/inject.rs`——手解析(core 无通用 regex,只
grep-regex)两种 `!cmd` 形态 + `@file` mention,`expand_slash_injections(body,cfg,cancel)`
建**最小 depth-0 ToolCtx**(SilentUi:注入无 UI 行,唯一交互是权限门经 approver)跑展开;
`commands::run` 在 `expand_body` 后调它,`Ok`→turn / `Err`→`SlashResult::message`(不跑
turn)。**`!cmd`**:`run_gated_bash` 走**与真 bash 调用同一** `check_call("bash",…)`(deny/
安全/ask/approver + sandbox_auto)再 `bash::bash_tool`——命令作者写的也不豁免(教训 19b);
deny/失败(spawn/timeout)抛错中止,非零退出照常内联输出(带 `[exit N]`,同 bash 工具);
多个标记顺序执行(不并发,避免审批提示抢终端)。**`@file`**:`@path` 解析到 `effective_cwd`,
是**现存文件**才注入(否则当 prose 留字面,`@someone` 天然不触发)、`read_path_blocked`
(plan 31,deny+敏感路径)命中则记 `[access blocked …]` 不注入(不泄密)、内容截 100KB 附在
prompt 末(mention 原样留)。扫 `@file` 用**原始 body**(非 `!cmd` 展开后),命令输出不能驱动
文件读。fast-path:`has_injections` 无标记则原样返(存量命令零改动、零副作用)。

**测试(全绿,+7)**:inject.rs — `find_embedded`(block+守卫 inline、`x!` `/`$!` 不匹配、
未终止/空跳过、行首允许)、`find_mentions`(前导字符守卫、`a@b.com` 不匹配、路径字符)、
`floor_char_boundary` 不切 UTF-8;commands/mod.rs — `!`echo hi`` 经门内联成 `Say hi to
world.`(参数替换先行)、`@notes.txt` 附文件内容(allow_all + 临时 cwd)、deny 规则挡
`!`rm nope`` → 无 turn + 报 `blocked by a deny permission rule`。

**真 key 验收(anthropic 轨,scratch cwd `--plain --permission-mode bypass`)**:命令
`brief.md` 含 `Bash says: !`echo LIVE-MARKER-42`. … @data.txt …`,`data.txt`=`SECRET-TANGERINE`
→ 模型回 `LIVE-MARKER-42 SECRET-TANGERINE`;rollout 实据:原始 `!`echo…`` 标记消失(执行→
内联)、`SECRET-TANGERINE` 出现 2 次(注入 + 模型转述)。**踩坑(教训)**:首跑得"没生效"假
象(rollout 里标记原样、无文件内容)——`target/debug/kloop` 是陈旧二进制,`cargo test`/
`build -p kloop-core`/`clippy` 都不产它;`cargo build -p kloop` 重建后即闭环。

## 完成记录(三片:注入统一到 `skill` 工具路径,提交 2446ee5)

二片只在 `/name` 用户路径展开注入,模型经 `skill` 工具激活的 skill 不展开——行为不一致,
也把 plan 28 的 `` !`cmd` `` 挂账悬着。三片补齐:`skill` 工具在 `expand_body` 后调
`inject::expand(&body, ctx)`(`skill` 工具本就持真 `ToolCtx`,不用 slash 路径的最小 ctx),
inline 返回展开后正文、**fork 在展开后再 fork**(子 agent 看到解析后的输出)、deny/失败的
`!cmd` 上浮成 is_error tool_result。**风险复核**:模型激活 skill 触发 bash 与模型直接调
bash 工具走同一权限门,无新风险面(二片挂它是切片纪律,机制验过即补);kloop 无远程/MCP
skill,cc 的"MCP skill 永不执行 `!cmd`"约束无对应物。落法极轻(inject::expand 改
`pub(super)` + `skill_tool` 加一行调用);`inject::expand` 自带 `has_injections` fast-path,
无标记的 skill 逐字节不变。**测试(+2)**:skill.rs — 模型激活的 skill 的 `!`echo INJECTED``
经门内联、deny 规则挡 `!`rm nope`` → is_error。**真 key 验收(anthropic 轨,`--headless
--permission-mode bypass`)**:`.kloop/skills/livecheck/SKILL.md`(catalog 可见,body 含
`!`echo SKILL-MARKER-99``)→ "Use the livecheck skill …" → 模型自发调 `skill` 工具、激活时
执行 bash、答 `SKILL-MARKER-99`;rollout 证原始 `echo` 标记消失、展开后正文进 tool_result。

## 未做(挂账,滚进后续片/plan)

- 命令 `allowed-tools` frontmatter 预授权其自身 `!cmd`(cc 注进本 turn
  `alwaysAllowRules`;kloop 现每个 `!cmd` 都过门,命令作者不能免批准)。
- `@file` 的 `#Lstart-end` 行范围、`@~/…` home 展开;`!cmd` 输出截断/超时精调(现抄 bash
  工具现值)。
- 子目录命名空间 `namespace:command`;commands 进 catalog(`disable-model-invocation`
  反向语义);`argument-hint`/命名参数 `$foo`(skills 侧一并挂账);managed/policy 层;
  命令里再调命令。
