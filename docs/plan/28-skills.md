# Plan 28 — skills(模型自选的可复用 prompt/资源包)

> 一句话定位:skills = **模型按描述自选**的 prompt 模板 + **渐进披露**(名+触发描述常
> 驻、正文触发前不进上下文)+ 可选**捆绑资源** + 可选 **fork 执行**。它不是从零的新机
> 制,而是缝合三个已有件:plan 23 的 inline 展开 seam、plan 16 的渐进披露门、plan 17
> 的 task 子 agent。剥掉"模型自选 + 渐进披露"两条,它就塌回 plan 23 的用户 slash 模板。

## 回源结论(三家真读,2026-07-14)

**2/3 收敛**(cc 全量 + claw 归档子系统),比当年 slash(只 cc 一家,plan 23)信号强。

1. **claude-code —— 完整 skills 系统(主参考)**。载体:`.claude/skills/<name>/SKILL.md`
   **目录格式**(已从单文件 `.md` 迁走,理由=捆绑 reference 文件);发现层同 slash 的
   四层(managed / 用户全局 `~/.claude/skills/` / 项目层向上遍历 / `--add-dir`)。
   **skill 与 slash 是同一个 `Command` 对象**——skill = 也能被模型经 `SkillTool` 按
   `when_to_use` 自选的 slash,外加渐进披露。16 个 frontmatter 字段:`name` /
   `description` / `when_to_use`(AI 匹配依据)/ `allowed-tools`(执行时并进权限白名单)
   / `argument-hint` / `arguments` / `model` / `effort` / `context`(`inline`默认 |
   `fork`)/ `agent` / `user-invocable` / `disable-model-invocation` / `version` /
   `paths`(条件激活)/ `hooks` / `shell`。**执行两路**(`SkillTool.call` 按
   `context` 分流):**inline** = 正文注入成 UserMessage 进主流(`$ARGUMENTS` 替换、
   `` !`cmd` `` shell 展开、`${CLAUDE_SKILL_DIR}`/`${CLAUDE_SESSION_ID}` 替换、
   `contextModifier` 并入 allowedTools/model/effort);**fork** = 独立子 agent 跑
   (`executeForkedSkill`,独立 token 预算,结果提取后释放子 agent 全部消息)。**渐进披
   露**:skill 清单注入 system 只占**上下文 1%**(≈8000 字符),单条描述上限 1536 字符
   (曾 250),三级降级(全描述→bundled 不截+非 bundled 均分→非 bundled 仅名字);正文
   只在 `SkillTool` 触发时载入。**条件激活**:带 `paths` 的 skill 平时进
   `conditionalSkills` 不可见,文件操作路径匹配才移入 `dynamicSkills`。**使用排名**:
   `score = usageCount × max(0.5^(days/7), 0.1)`,7 天半衰期、0.1 保底、60s 去抖。
   **权限**:五层(deny → 远程 canonical 放行 → allow → Safe-Properties 白名单 30 项
   → ask),正向安全(未来新属性默认要权限)。**bundled**:编译期打包,首调才把
   `files` 懒解压到临时目录(`O_NOFOLLOW|O_EXCL` 防符号链接攻击)。远程加载
   (`EXPERIMENTAL_SKILL_SEARCH`,`gs://`/`https://`/`s3://`,不过 `$ARGUMENTS`/`!cmd`)。
2. **claw-code —— 有 skills 子系统(已归档占位)**。`skills.json` 记 20 模块:
   `skills/loadSkillsDir.ts`、`skills/bundledSkills.ts`、`skills/mcpSkillBuilders.ts` +
   `bundled/{verify,simplify,loop,remember,skillify,updateConfig,debug,batch,...}.ts`。
   形态与 cc 同源(bundled + 磁盘 + MCP 三来源),细节已归档不可精读——**收敛靠"存在
   同名子系统"而非逐字段核对**。
3. **codex —— 无**。同 slash:codex 只有内置系统提示模板,无用户可编排的
   能力包。

**脚本执行的实测定位(2026-07-14 真扫 cc 仓)**:cc 实际的 skill(`verify` /
`interview` / `teach-me`)全是 `SKILL.md` + 顶多 `examples/*.md`(参考文档),全仓**无
一**带 `.py`/`.sh` 脚本子目录——印证"skill 本质是 prompt + 参考,不是可执行代码"。真带
脚本时,"执行"分两条、**都不是 skill 机制自己跑**:①正文引用的
`${CLAUDE_SKILL_DIR}/scripts/*`——`SkillTool` inline 分支实测 = `createUserMessage` 纯注
入、**零 spawn/exec**,脚本靠**模型自发一个 Bash 工具调用**去跑(经 shell;脚本可任意语
言 `python`/`node`,解释器由命令/shebang 定;走 plan 8 权限门)。②`` !`cmd` `` 内联命令
才是"机制在展开时同步跑 shell、把 stdout 嵌进消息",但那是 slash 的 attachment 机制
(`attachments.ts`,引 `BASH_TOOL_NAME`),本 plan 挂账不做。对照 claw:其 bundled skill
是编译进程序的 `.ts` 代码模块(`bundled/verify.ts`),"skill = 代码"直接跑 TS、不经
bash——但那是编译期打包形态,和"用户往 skill 目录放脚本"两回事。**推论**:kloop 片 1
**不必为脚本执行做任何特殊机制**,`${CLAUDE_SKILL_DIR}` 路径注入 + 已有 bash 工具门
(plan 8)就够,别过度设计。

**教训 11 点**:①plan 备忘曾把 skill 当"slash 的另一个功能",实测是**同一 `Command`
对象的两个触发面**(用户 `/name` vs 模型 `SkillTool`)——这决定 kloop 该把 plan 23 挂账
的"用户自定义 slash 模板"和本 plan 合起来做,而非并列两套载体。②cc 单条描述上限从 250
涨到 1536,说明**清单描述是给模型做匹配的**、正文才是能力——kloop 复用 plan 16 discover
门时,常驻的应是 `when_to_use`(匹配用),不是正文。

## 目标

用户在 `.kloop/skills/<name>/SKILL.md` 定义一段带描述的可复用 prompt(可带捆绑文件);
**模型按 `when_to_use` 自发选它**(渐进披露:平时只有名+描述在上下文,触发才载入正文并
展开成 user 消息继续跑),用户也能 `/name args` 手动调。与 plan 23(用户手输 slash 模
板)、plan 17(`[agents.<name>]` 命名子 agent)是同一"命名定义 + 参数"家族里的**模型
触发 + 渐进披露**这一格。

## 与已有件的缝合关系(实现骨架,非另起炉灶)

- **inline 展开** = plan 23 `commands/mod.rs` 已预留的 seam(mod 注释原话:用户模板
  "plug into this same seam — a `custom.rs` sibling + a lookup ahead of `run`'s
  match";`is_command` 解析已拆出 argument 串)。skill 的 inline 正文替换后即成一条
  user 消息进 History,和用户 slash 模板走同一条替换 + 注入路。
- **渐进披露/模型触发** = plan 16 `discover.rs` 的 `unlocked_tools` + `deferred_notice`
  + 打分门。**关键决定见下**:是把每个 skill 当一个 deferred"能力"挂进这套门(name +
  `when_to_use` 进 notice、触发时载入正文),还是照 cc 平行开一个 `SkillTool` + 把清单
  注入 system(预算截断那套)。kloop 已有 discover,倾向复用;但 skill 的"激活一段
  prompt"语义 ≠ tool 的"解锁一个 def",别硬塞。
- **fork 执行** = plan 17 task 递归复用 `run_turn`(深度限 1、独立预算、Config clone
  继承权限)。`context: fork` 的 skill 直接走 task 那条子 agent 路。
- **参数替换** = plan 23 挂账的 `$ARGUMENTS`(整串)/ `$N`(**0 索引**,shell-quote
  拆词,教训 11 已纠正)——本 plan 需真正落地(plan 23 内置面没做替换),两个触发面共用。
- **脚本执行** = 已有 bash 权限门(plan 8),**skill 机制本身不执行脚本**。正文里的
  `${CLAUDE_SKILL_DIR}` 替换成 skill 目录绝对路径(指向捆绑的 `scripts/*`);要不要跑、
  怎么跑由**模型用 bash 工具**决定,危险命令 / 敏感路径 / deny 规则照旧生效。所以"带脚
  本的 skill"片 1 即可用(目录格式 + 路径注入就够,脚本就放在 skill 目录里),**不需要
  bundled 懒解压**那套——那是编译期打包 skill 才要的(仍挂账)。
- **清单注入** = plan 13 `context.rs` 把项目指令合成每请求首条 user 消息的同类缝(若走
  "注入 system/首 user"路而非 discover 门)。

## 关键决定(开工时定 / 问用户)

1. **载体格式 + frontmatter 依赖**(最大张力,必问):`.kloop/skills/<name>/SKILL.md`
   目录格式(几乎被"捆绑资源"强制)几乎又强制 frontmatter 结构化字段(`when_to_use` /
   `allowed-tools` / `context` / `model`)。但 kloop 一路在**躲 YAML 依赖**(plan 17 片
   2 agents 选 config.toml、plan 23 slash 描述取首行,都为躲 serde_yaml)。三条路:
   (A) 自写极简 frontmatter 解析(只认 `key: value` + 简单 list,不引 serde_yaml,像
   cc);(B) 目录 + `skill.toml`(躲 YAML,但和"md 正文即模板"的直觉别扭);(C) 复用
   config.toml `[skills.<name>]`(和 agents 一致,但捆绑资源无处放、正文塞进 toml 字符串
   很挤)。倾向 (A):skills 的价值恰在结构化描述 + 捆绑,躲 YAML 的理由这次最弱。
2. **模型触发面**:复用 plan 16 discover 门(省事、已缓存友好)vs 平行 `SkillTool` +
   system 清单注入(照 cc,预算 1% 截断)。二者语义差:discover 是"解锁 def",skill 是
   "激活 prompt"。开工时定,倾向先复用 discover 的**打分 + 触发载入**骨架,注入形态按
   skill 语义调(触发返回的是展开后的 user 消息,不是工具 def)。
3. **执行默认**:inline 为默认(最小、和用户 slash 同路);`context: fork` 复用 plan 17
   task。片一可先只做 inline,fork 挂账。
4. **user-invocable / disable-model-invocation**:是否两个开关都要。最小版:默认两面都
   可(用户 `/name` + 模型自选),开关挂账。

## 不做(挂账,记为后续可能性)

bundled 编译期打包 + 懒解压;`paths` 条件激活;使用频率指数衰减排名;远程加载
(`gs://`/`s3://`);MCP skills(server prompt → skill);`hooks` / `shell` frontmatter;
预算三级降级截断(kloop 若走 discover 门则天然不需要);`allowed-tools` 权限白名单注入
(先不动权限,skill 继承调用点权限);`` !`cmd` `` frontmatter 内联 shell 展开(先纯
`$ARGUMENTS`/`$N` + `${CLAUDE_SKILL_DIR}` 文本替换);命名空间/子目录。

## 建议切片

- **片 1**:磁盘 skills(`.kloop/skills/<name>/SKILL.md`,决定 1 定的载体)+ 极简
  frontmatter(name / description / when_to_use)+ **inline 执行** + **模型触发**(复用
  discover 门,`when_to_use` 常驻)+ **用户 `/name` 手调**(接 plan 23 seam)+
  `$ARGUMENTS`/`$N` 替换落地 + **`${CLAUDE_SKILL_DIR}` 路径注入**(替换成 skill 目录绝
  对路径,让"带脚本的 skill"能指到 `scripts/*`——脚本由模型用 bash 跑,走 plan 8 权限
  门,skill 不自己执行)。这一片打通"模型自选 + 渐进披露"的独有生态位(教训 18)。
- **片 2**:`context: fork` 复用 plan 17 task;`allowed-tools` 注入权限;`model`/`effort`
  覆盖。
- **片 3+**:bundled / 条件激活 / 使用排名 / MCP skills(按需)。

## 测试

frontmatter 极简解析(有/无 when_to_use、多余字段忽略、坏 YAML 不崩);发现层(项目
`.kloop/skills/` + 全局 `~/.kloop/skills/`,重名优先级);渐进披露(未触发时只有名+描述
进上下文、正文不进;触发后正文载入并展开成 user 消息);模型触发(给贴 when_to_use 的任
务、**不点名 skill**,模型自选——教训 18 的"自发采用"验法);用户 `/name args` 手调等价
展开;`$ARGUMENTS`/`$N`(0 索引)替换 + 无占位符且有参时追加 `ARGUMENTS:`;未知
`/skill` 列清单;缺参数处理;与内置 slash(plan 23)、`[agents.*]`(plan 17)不冲突。

## 完成标准

cargo fmt + clippy + test 全绿,一次 commit(写清验证方式,真 key 验"模型不点名自选
skill"这一条——教训 18);README 同步 skills 用法与载体格式;本文件补完成记录(提交
号 + 挂账);HANDOFF.md 补教训(尤其 frontmatter 依赖这次躲不躲得掉、以及 skill 复用
discover 门的语义边界)。
