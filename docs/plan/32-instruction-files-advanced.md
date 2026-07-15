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

## 完成记录(2026-07-15,提交 f267305)

**做了片 1(@import)+ 片 2(rules 目录 + local 覆盖);片 3(子目录懒加载)挂账。** 用户
拍板"都做吧"= 片 1+2(片 3 明确不做,三家里只 cc 做、与合成 user 消息缝不匹配)。全改动落
**发现层** `cli/src/context.rs`(IO),core 只加一档 `InstructionScope::Local` + 标签,
`assemble_instructions` 纯组装逻辑不变——落点分工与本文件预判一致。

**回源纠偏(两处,教训 11 再验)**:
1. plan 备忘写"claw 无"——**错**。真读 `refs/claw-code/rust/crates/runtime/src/prompt.rs`
   `discover_instruction_files`:claw 有 `.claw/rules` + `.claw/rules.local` 目录 + 逐目录
   `CLAUDE.local.md`(叠加式),只是**没有 `@path` 递归展开**。所以 **片 2(rules+local)是
   cc+claw 双家收敛**,比原判"cc 一家"信号强;`@import` 才是 cc 独有。
2. plan 备忘写 @import"被引在前主文件在后"(引 cc 头部旧 docstring)——**与 cc 现行代码相反**。
   `claude-code/src/utils/claudemd.ts:660-663` `processMemoryFile` 实际**先 push 主文件、再
   push 被引**(注释 "parent before children")。kloop 按真实代码:**主文件在前、@import 内容
   紧随其后**(后者权重略高,符合 kloop"越靠后越重"),对应测试 `import_expands_with_the_main_file_first`。
3. codex 侧坐实:`codex-rs/core/src/agents_md.rs` 只做 root→cwd 拼接(kloop 已有形态)+
   `project_doc_max_bytes`,**无 @import/rules/子目录懒加载**;它的 local 概念是 `AGENTS.override.md`
   (作候选文件名排在 AGENTS.md 前,命中即**替换**该目录 AGENTS.md,非 cc 叠加式——分歧,kloop 取 cc 叠加)。
4. **片 3 决断有据**:子目录懒加载 cc 独有,codex/claw 都 root→cwd 全量、明确不做 → 弱收敛 +
   缝不匹配,挂账。

**开工决定(按 plan 倾向定)**:① @import 两种指令文件(AGENTS.md/CLAUDE.md)都认;无显式转义,
走 cc 式"@ 在行首/空白后 + 合法路径首字符"(`a@b.com`、prose `@someone` 天然不触发);② rules 目录
= `.kloop/rules/*.md`(与 kloop 配置目录一致);③ local 文件 `AGENTS.local.md`/`CLAUDE.local.md` 都认
(前者胜,同主文件回退);④ 片 3 挂账。

**实现要点**:
- 每目录顺序 = 主文件(+@import) → `.kloop/rules/*.md`(sort,各自 +@import) → local(+@import);
  全局层只主文件(+@import),不扫 rules/local(全局 `~/.kloop/` 与项目 `dir/.kloop/` 路径结构不对称,
  且 plan 把 rules/local 定在 Project 层——保持最小,全局要拆分用 @import)。
- `@import` 深度 `MAX_INCLUDE_DEPTH=5`(`depth>=5` 停,同 cc,链载 depth 0..4);`processed:
  HashSet<PathBuf>`(canonicalize 后的 key)去重 + 断环,**跨整个发现过程共享**;canonicalize 兼做
  symlink realpath + 读盘目标。
- 外部边界:项目/local 的 import 只准落 git root(无 git root 则 cwd)子树内,越界跳过 + 告警;
  **全局层 import 无边界**(对齐 cc User memory `includeExternal=true`)。cc 用 process cwd 作边界、
  且有审批流;kloop 用 git root 子树 + 无审批直接跳(单人工具,取更宽松合理的边界,记为偏离)。
- 缺失 import 告警**仅当 spec 文件样**(含 `/` 或 `.`):`@./typo.md` 告警、prose `@someone` 静默
  (cc 全静默,kloop 加保守告警但压住 prose 噪声——偏离 cc,理由:给 typo 反馈又不刷屏)。
- `@import` 抽取:逐行扫,跳过围栏代码块(``` / ~~~),`split_whitespace` 后 `@` 开头的 token 即
  "行首/空白后"(邮件天然排除);strip `#fragment`;`is_valid_import_spec` 复刻 cc 接受规则。
  **未做**(偏离 cc):inline codespan(单反引号)内的 @path 不特判(只跳围栏块);markdown lexer
  不引入(纯行扫,零新依赖)。

**验证**:cargo fmt + clippy(-D warnings 干净)+ test 全绿(494 通过,context 模块新增 11 测
+ core 1 测)。端到端:临时 git 仓库(AGENTS.md @import 一真一缺 + `.kloop/rules/{10,20}` + notes.txt
+ AGENTS.local.md)`--plain` 跑通——缺失 import 精确一条启动告警、prose `@someone` 不告警、不崩。
**真 key 验证(可选那条,anthropic 轨)**:sonnet-5 一问确认四个只可能来自 @import/rules/local 的
标记短语(`two-space indent`/`10-first`/`20-second`/`PRIVATE local override`)全部在其项目指令里——
三特性端到端到达模型、gather() 组合顺序正确。

**挂账(不做,附依据)**:
- **片 3 子目录懒加载**:cc 独有(conditionalRules/`getMemoryFilesForNestedDirectory` + frontmatter
  `paths` glob),codex/claw 均不做;要在工具执行(read/edit 命中某目录)时动态往"每请求刷新的合成
  user 消息"加该目录 memory,与 cc 的 attachment 机制不同缝,代价大、收益弱。
- `claudeMdExcludes` 排除设置、enterprise/managed 层、@import cwd 外白名单细粒度审批(kloop 直接跳)、
  内存文件自动写入、全局 `~/.kloop/rules/`(路径结构不对称,用 @import 替代)、frontmatter 条件规则
  (`paths` glob 属片 3 机制)、inline codespan @path 特判。
