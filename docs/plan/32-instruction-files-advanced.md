# Plan 32 — 指令文件高级特性(@import / rules 目录 / local 覆盖 / 子目录懒加载)

> 一句话定位:plan 13 只做了"AGENTS.md 为主、CLAUDE.md 回退 + 全局/项目两层 + 32KiB 预
> 算 + 合成 user 消息";cc 的 `@import` / `.claude/rules/*.md` / `CLAUDE.local.md` / 子目
> 录懒加载都记了"未来可能性"。本 plan 按需补这几个发现层/组织层扩展。**优先级中低**——
> 单人早期工具,长 AGENTS.md 直接写也够;有"指令拆分/私有覆盖"痛感再上。

## 回源结论(2026-07-14,`src/utils/claudemd.ts`)

**基本 cc 特有**(CLAUDE.md 生态);codex 用 AGENTS.md 但**无** @import/rules/
local(只 `project_doc_max_bytes` 单文件);claw **无**。→ 收敛信号弱、形态抄 cc 一家
(同 slash plan 23 的处境,教训 11:单参考、按需裁剪)。

1. **@import**(`processMemoryFile` 递归,claudemd.ts:617+):指令文件里 `@path` 引用递
   归展开;`MAX_INCLUDE_DEPTH = 5`;`processedPaths` set **防循环/去重**;**includes 排在
   主文件内容之前**再拼主文件;**cwd 外的路径默认不导入**(`includeExternal` 开关);
   symlink 先 realpath 解析;总量仍受 `MAX_MEMORY_CHARACTER_COUNT = 40000` 约束。
2. **层级**(claudemd.ts:6-16):User `~/.claude/CLAUDE.md` → Project(`CLAUDE.md` +
   `.claude/CLAUDE.md` + **`.claude/rules/*.md` 全部 .md**)→ Local **`CLAUDE.local.md`**
   (私有、gitignored、不签入)。`claudeMdExcludes` 设置可排除指定文件。
3. **子目录懒加载**(conditionalRules,attachments.ts:1866-1906 + memoryFileDetection):
   开场不全量加载整棵树;**读/编辑某子目录下文件时**,才按需把那个目录的 memory 注入上
   下文。省 token,让深层目录的局部规则只在相关时出现。

## kloop 现状与落点(已读准,`crates/core/src/context.rs` + `crates/cli/src/context.rs`)

- 纯组装在 core、IO(发现文件/git)在 cli(`core` 无网络无 IO 边界)。
- `INSTRUCTIONS_MAX_BYTES = 32*1024`(:19,codex `project_doc_max_bytes`);
  `assemble_instructions`(:100)按 **global → git root → cwd** 顺序拼、总预算截断/跳过;
  `InstructionScope::{Global, Project}`(:44-46)。
- AGENTS.md 为主、CLAUDE.md 同目录回退;指令走**合成 user 消息**(`<project-instructions>`,
  :137)、每请求刷新、不进 system/History。
- **落点分工**:@import 展开是 **IO**(读被引文件)→ 落 cli/context.rs 的发现层,core 的
  `assemble_instructions` 拿到的仍是"已展开的文件内容列表",纯组装不变;rules 目录/local
  文件同理是发现层多扫几个路径;子目录懒加载要碰工具执行路径(见片 3 张力)。

## 建议切片(按实用度)

- **片 1(@import,最实用最小)**:指令文件内 `@path` 递归展开,深度限 5、`processedPaths`
  去重防循环、被引在前主文件在后、cwd 外默认不引、symlink realpath;展开后仍进 32KiB 总
  预算。**纯发现层 IO**(cli/context.rs),core 组装不变。单人把长 AGENTS.md 拆成多文件的
  刚需。
- **片 2(rules 目录 + local 覆盖)**:Project 层多扫 **`.kloop/rules/*.md`**(全部 .md,
  排序稳定)；加 **Local 层 `AGENTS.local.md`/`CLAUDE.local.md`**(gitignored 私有覆盖,排
  在最后=最高优先，同 cc)。都只是发现层多加路径 + `InstructionScope` 加一档 Local。
- **片 3(子目录懒加载,最复杂、优先级最低)**:读/编辑某子目录文件时注入该目录 memory。
  **张力**:kloop 指令是"每请求刷新的合成 user 消息",懒加载要在工具执行(read_file/
  edit_file 命中某目录)时动态往指令集加该目录文件——和 cc 的 attachment 机制不同缝,代
  价大。倾向**记挂账**,除非有真实深目录规则需求。

## 关键决定(开工时定 / 问用户)

1. **@import 语法引入非标准 AGENTS.md**:`@path` 是 cc 对 CLAUDE.md 的扩展,AGENTS.md 规
   范本身无此语法。kloop 给自己的指令文件(AGENTS.md + CLAUDE.md 都算)加 `@import` 是否
   接受?倾向接受(kloop 自有扩展、文档说明;和 codex `project_doc_max_bytes` 单文件不
   冲突)。转义:字面 `@` 怎么写(cc 有无转义?开工核对)。
2. **rules 目录位置**:`.kloop/rules/*.md`(kloop 配置目录一致,**倾向**)vs `.claude/
   rules/`(cc 兼容)。
3. **local 文件名**:`AGENTS.local.md`(和主文件 AGENTS.md 对齐,倾向)vs `CLAUDE.local.md`
   (cc 兼容)。可两者都认(和 AGENTS/CLAUDE 回退同理)。
4. **片 3 做不做**:倾向本 plan 只做片 1(+片 2),片 3 挂账。

## 不做(挂账)

子目录懒加载(片 3,除非选做);`claudeMdExcludes` 排除设置(有痛感再加);enterprise/
managed 层(kloop 无托管场景);@import 的 cwd 外白名单细粒度(默认不引外部即可);内存文件
的自动写入/编辑(cc 的 memory 写入命令,非本 plan)。

## 测试

@import:单层展开(主文件 + 被引内容、被引在前)、深度限 5 截断、循环引用不死循环(A→B→A)、
cwd 外默认不引、被引文件缺失不崩(告警跳过)、展开后超 32KiB 预算截断;rules 目录:
`.kloop/rules/*.md` 全扫 + 稳定排序;local:`AGENTS.local.md` 进 Local 层且排在最后(优先级
最高);与 plan 13 现有(AGENTS/CLAUDE 回退、global/project 分层、合成 user 消息)不回归。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 同步 @import/rules/local 用法与优先级;本文件补
完成记录(提交号 + 挂账,尤其片 3 是否做);HANDOFF 补教训(@import 引入非标准语法的取舍、
懒加载与"合成 user 消息"缝的不匹配)。真 key 非必需(纯发现层组装,单测足够;可选验一次
`@import` 拆分的 AGENTS.md 被模型正确遵守)。
