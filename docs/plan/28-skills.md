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

## 完成记录(片 1,提交 <pending>)

**用户开工拍两点**:①载体必须是**公开 Agent Skills 规范**的 `SKILL.md`(目标"下载了就
能用"),直接否掉 (B) `skill.toml`;②**用 YAML 库**(不自写解析),选 `serde_yaml_ng`
(archived 的 serde_yaml 的活跃 fork,纯 Rust `unsafe-libyaml`,`#[derive(Deserialize)]`
未知字段天然忽略)。**这纠正了 plan 一处二手错**:cc 内部 16 字段里的 `when_to_use` 不是
公开规范字段——真·下载来的 skill 只有 `name`+`description`,**`description` 才是模型匹配
信号**(既是"做什么"又是"何时用")。渐进披露常驻的因此是 `description`,不是 `when_to_use`。

**落地(片 1 全绿,`kloop/` 下 `cargo fmt+clippy+test` 383 测试通过)**:

- **core `skills.rs`**(纯半,仿 `context.rs`——CLI 做 IO、这里纯解析/组装/展开):`Skill`
  {name,description,body,dir};`Skill::parse`(`split_frontmatter` 认 `---…---` 栅栏 + serde_yaml_ng
  解析 frontmatter,name 缺省=目录名、description 缺失/空=Err→CLI 跳过告警);`Skill::lookup`
  (仿 AgentType,miss 列可用清单,model-facing);`skills_catalog`(渐进披露注入块,搭
  `injected_context`);`skill_tool_def`(仅有 skill 时注册);`expand_body`
  (`$ARGUMENTS`/`$N` 0 索引 shell 拆词/`${CLAUDE_SKILL_DIR}` + 无占位符且有参→追加
  `ARGUMENTS:`);`skill_tool`(查表+展开,**回灌成 tool_result** 让正文进上下文续跑——
  比 cc 的"queue 一条 user 消息"更省一个机制)。
- **接线**:`Config.skills: Arc<Vec<Skill>>`(随 clone 继承);`injected_context` 三段
  (instructions + skills_catalog + deferred_notice,顺序稳定=cache 稳);`turn_rounds` 在
  all_tool_defs 后、allowlist retain 前 push `skill` def(不计入 defer 阈值、不进 run_program
  TS——它是"激活 prompt"缝不是 source 工具;放 retain 前故受限 agent_type 可 gate);
  `execute_tool`/`is_concurrency_safe`/`permissions::CallFacts::is_readonly` 各加 `skill`
  =readonly 自动放行。
- **用户 `/name`**:`SlashResult` 加 `run_turn: Option<String>`(附三个私有构造器
  message/cleared_message/turn,清掉四个 builtin 的字面量);`commands::run` builtin miss →
  `Skill::lookup` 命中则 `turn(expand_body)`、否则 `unknown`(现也列 skills);三前端处理
  `run_turn=Some`——plain(`line=prompt` 落回 turn 路)、TUI worker(record+run_turn+TurnEnded)、
  server(record+run_turn+turn/completed;抽 `turn_completed_params` 复用)。
- **CLI 发现层**:`load_skills(cwd)` 找 `.kloop/skills`(cwd)+ 全局 `~/.kloop/skills`,
  **只扫 kloop 自己的 `.kloop/`、不扫 cc 的 `.claude/`**(下载即用靠 SKILL.md 格式合规,
  拷进 `.kloop/skills` 即可);拆出 `skills_from_roots`(纯,hermetic 可测:优先级+去重+告警);
  `skills` 穿进 config_from_env/plain_main(两函数补 `#[allow(clippy::too_many_arguments)]`,
  仿 hooks.rs/agent.rs 既有先例)+ 三 config 工厂;--mock 空。

**真 key 验收(anthropic 代理轨,sonnet-5)**:①样例 `haiku` skill(description 只说"写
haiku/短诗"),prompt「write me a short poem about the autumn moon」**不点名 skill** → 模型
自发 `skill({"name":"haiku","arguments":"autumn moon"})`、输出带 skill body 独有的
`HAIKU-BY-SKILL:` 标记 + 5-7-5(教训 18 的"自发采用"铁证);②`/haiku a quiet sunrise` 手调
→ 同样展开跑通出正确 haiku(观察:模型收到展开的 user 消息后**又调了一次 skill 工具**——因
catalog 里也列着 haiku、它把"写 haiku 请求"正式"激活"一遍,冗余但无害、结果正确;可接受)。

**教训**:
1. **plan 备忘的 frontmatter 字段是 cc 内部实现、非公开规范**——`when_to_use` 是 cc 私有,
   规范只有 `name`+`description`(description 兼任匹配信号)。回源看"cc 源码有什么"≠"生态
   规范是什么";"下载即用"这类兼容目标要对着**公开规范**核字段,不是对着某家实现。
2. **躲 YAML 依赖的理由这次最弱、该躲反而错**:skills 价值恰在结构化描述 + 生态兼容,任意
   合规 frontmatter 都要能解析,自写解析器会在冷门 YAML 上崩掉破坏"下载即用"承诺——引
   `serde_yaml_ng` 是对的(kloop 第 2 个为功能引入的依赖,继 similar)。
3. **skill 复用 discover 门的语义边界**:没有硬塞进 tool_search/unlocked_tools。skill 是
   "激活一段 prompt"、discover 是"解锁一个 def",两者语义不同。取的是 discover 的**注入骨架**
   (catalog 搭 `injected_context`、会话稳定 cache 友好),触发另起一个专用 `skill` 工具、
   回灌**展开后的正文**(tool_result)而非 tool def。inline 执行不需要新注入机制——tool_result
   进上下文续跑即可,比 cc 的 user-message 注入更省。
4. **运行期 cwd 陷阱(验收踩)**:skills 按进程 cwd 发现;真机 workspace 在 `kloop/` 子目录、
   env.local+样例 skill 在仓库根,`cd kloop` 后 cwd 变了 skill 就找不到。直接跑
   `./kloop/target/debug/kloop`(cwd=仓库根)才对齐。

**片 1 挂账**(按 plan「不做」节 + 片 2/3):`context: fork` 走 task 子 agent;`allowed-tools`
注入权限、`model`/`effort` 覆盖;bundled 编译期打包 + 懒解压;`paths` 条件激活;使用频率衰减
排名;远程(`gs://`/`s3://`)/MCP skills;`hooks`/`shell` frontmatter;`` !`cmd` `` frontmatter
内联 shell 展开;`${CLAUDE_SESSION_ID}` 替换(片 1 只做 SKILL_DIR)。

## 完成记录(片 2,提交 <pending>)

**scope 收敛(开工核对 plan 的自相矛盾)**:plan 片 2 列 `context: fork` + `allowed-tools` +
`model`/`effort`,但**「不做」节又把 `allowed-tools 权限白名单注入` 列为挂账**("先不动权限,
skill 继承调用点权限")——plan 自相矛盾。核算后定片 2 = **`context: fork` + `model` 覆盖**,
`allowed-tools`/`effort` 挂账,依据:
- **`allowed-tools` 挂账(关键判断)**:公开 skill 用的是 **cc 工具名**(`Read`/`Bash`/`Grep`),
  与 kloop 的 `read_file`/`bash`/`grep` 不对应。若当**限制**(tool_allowlist)注入,下载来的
  skill 的 `allowed-tools:[Read,Bash]` → kloop 白名单 {Read,Bash} 一个都不匹配 → 子 agent
  除 read_offloaded 外**无工具**,直接**破坏"下载即用"**。若当**授权**(自动放行)注入,则是
  供应链风险(下载的 skill 声明 `[bash]` 就免询问跑任意命令)。两条都不该草率做——留片 3,
  要做得先定 cc→kloop 工具名映射表。这也和「不做」节自洽。
- **`effort` 挂账**:是 provider 层参数(responses 的 reasoning / anthropic 的 thinking),
  烘焙进 Provider Arc,per-skill 覆盖要重建 provider,plumbing 重、价值低。

**落地**:
- `skills.rs`:`SkillContext{Inline(默认)|Fork}` + `Skill.context`/`Skill.model`;`Frontmatter`
  加 `context`/`model`(`context:"fork"`→Fork,其余→Inline;model 空→None);`Skill` 派生
  `Default` 便于测试字面量 `..Default::default()`。`skill_tool` 按 `context` 分流:Inline 回
  展开正文(片 1 原样),Fork 调 `crate::tools::fork_skill`。
- `task.rs`(**DRY 重构**):抽 `run_sub_agent_sync`(同步派子 + outcome 映射,`who` 前缀
  错误)与 `clone_for_subagent`(fresh todos/inbox 的克隆不变量)+ `next_agent_label`;
  `task_tool` 与新 `fork_skill` 共用。`fork_skill`:body 作子 agent prompt、`model` 覆盖、
  UI 标签 `[skill:<name>]`;**depth≥1 退化 inline**(子 agent 不能再 spawn,同 task 深度规则,
  但不 dead-end);`tools/mod.rs` `pub(crate) use task::fork_skill` 供 skills 调。
- **skills 收敛为 depth-0 only(fork 观察反推的改进)**:片 1 原为 all-depth(catalog 搭
  `injected_context` 全深度、skill 工具全深度)。真 key 跑 fork 发现子 agent **又看到 catalog、
  又 re-trigger 了自己那个 skill**(depth-1 inline 退化兜住了、无害但费一轮 + catalog 灌进
  **每个**子 agent 每请求=真 token 浪费)。skill 本是**顶层编排特性**(同 `task`/`run_program`
  都 depth-0),故 `turn_rounds` 的 skill 工具注册与 `injected_context` 的 catalog 都 gate
  `depth==0`(`sample_with_retry` 加 `depth` 参;`deferred_notice`/project_instructions 仍全深度
  不动)。改后 fork 子 agent 直奔 bash、不再 re-trigger。
- `/name` 手调**始终 inline**(不看 context):fork 是"模型委派、隔离 token"的关注点,用户
  亲手 `/name` 是要在自己会话里看结果,inline 才符合直觉——记为**有意与 cc 分歧**。

**真 key 验收(anthropic 代理轨,sonnet-5)**:`wordcount` fork skill(context:fork,body 让
用 bash `wc -w` 数词、只回数字),prompt「how many words are in '…'」**不点名** → 模型自发
`skill({name:wordcount})` → **派 `agent-1`(`[skill:wordcount]`)** → bash 在**子 agent 内**跑
(`[agent-1 · bash …]`)→ `[agent-1 finished]` → 父只拿到结果报数(中间 bash 输出不进父上下文)。
depth-0 gating 生效:子 agent **不再 re-trigger**(改前会多一条 `[agent-1 · skill …]`)。

**教训**:
5. **plan 自身可能自相矛盾,开工要通读核对、别只读「建议切片」**:片 2 建议列 `allowed-tools`,
   「不做」节又把它列挂账。遇到这种要停下判断而非硬做——这里的判断锚点是"下载即用"承诺:
   `allowed-tools` 用 cc 工具名,当限制注入会让下载的 skill 子 agent 无工具、当授权注入是供应
   链风险,两头都破承诺,故留片 3(要做先定工具名映射)。教训 11(plan 二手)的推论:plan 内部
   不同节的结论也可能打架,以"核心承诺 + 现有架构"仲裁。
6. **一个"能力全深度可用"的设计,真机跑一次才看出它的隐藏成本**:skills 片 1 图省事设 all-depth
   (catalog 全深度注入),单测全绿看不出问题;fork 片 2 真机一跑才暴露"子 agent 也灌 catalog +
   fork 子 agent 自我 re-trigger"——catalog 进每个子 agent 每请求是持续 token 浪费。收敛为
   depth-0(顶层编排特性,同 task/run_program)后既省 token 又去掉 re-trigger。判据:给一个新
   特性定"作用域/深度"时,先问"它是顶层编排概念还是每层都真需要";拿不准就跑一次多 agent 场景
   看它在子 agent 里的实际开销。教训 21(先看现有信号)的邻居:先看现有同类特性(task/run_program)
   的作用域,新特性八成同档。

**片 2 挂账**:`allowed-tools`(需 cc→kloop 工具名映射表 + 限制 vs 授权定性)、`effort`(provider
烘焙);其余同片 1 挂账(bundled 懒解压 / `paths` / 使用排名 / 远程 + MCP skills / `hooks`/`shell` /
`` !`cmd` `` / `${CLAUDE_SESSION_ID}`)。

## 完成记录(片 3,提交 <pending>)

**scope**:片 2 挂账的 `allowed-tools` 落地(用户"继续"点名这项)。定性 = **限制**(scope 子
agent 工具集,复用 `tool_allowlist`),**非授权**(不注入自动放行——那是供应链风险,片 2 教训 5)。
只作用于 **fork skill**(inline 在主上下文跑、全工具,限制无意义,忽略)。

**落地**:
- `skills.rs`:`Skill.allowed_tools: Option<Vec<String>>`;`Frontmatter.allowed-tools` 收
  `AllowedTools{List|Str}`(untagged;list 或 逗号/空格串,两形态生态都有);`map_tool_name`
  **cc→kloop 工具名映射**(Read→read_file / Bash→bash / Grep→grep / Glob→glob / Write→write_file
  / Edit→edit_file / WebFetch→web_fetch / WebSearch→web_search / Task→task / TodoWrite→todo_write
  / BashOutput→bash_output / KillShell|KillBash→kill_bash / Skill→skill;**未知名透传**——
  kloop 原生名 `read_file`、MCP 名 `srv__x` 直接匹配;cc scope 限定 `Bash(git:*)` 剥成 `bash`,
  kloop 靠权限规则 scope 命令、不靠 skill 工具表)。
- `task.rs` `fork_skill`:`allowed_tools` → 子 agent `tool_allowlist`(与 agent_type.tools 同款
  限制;`read_offloaded` 恒可用不受限,`agents::tool_available` 的基建例外)。

**真 key 验收(anthropic 轨,sonnet-5)**:`filecheck` fork skill(context:fork,
`allowed-tools:[Read,Grep,Glob]`,body 让读文件报行数),prompt「how many lines does CLAUDE.md
have」不点名 → 派 `agent-1` → **子 agent 用 `read_file`**(allowed)、**未碰 bash**(被限制掉)→
返结果;父(全工具)随后自发 `bash wc -l` 复核——**证明限制精确 scope 到子 agent、父不受限**。

**教训**:
7. **生态兼容的"名字映射"要双向留活口:已知名映射、未知名透传**。`allowed-tools` 用 cc 工具名,
   若只做"映射表命中才收、未命中丢弃",kloop 原生名(`read_file`)和 MCP 名(`srv__x`)会被误丢
   → 限制过紧废掉 skill。正解 = 映射已知 cc 名 + 未知名原样透传(既认下载来的 cc 名、也认 kloop
   原生/MCP 名),再 fail-soft(限制过紧只是少个工具、模型报"用不了",不静默腐蚀)。判据:跨生态
   翻译一层标识符时,别做封闭映射(白名单丢弃),做开放映射(命中改写、未命中透传)。这是片 2
   教训 5(cc 名≠kloop 名会破"下载即用")的正解补全。

**片 3 挂账**:`effort`(provider 烘焙)、bundled 懒解压、`paths` 条件激活、使用频率排名、
远程 + MCP skills、`hooks`/`shell` frontmatter、`` !`cmd` `` 内联 shell、`${CLAUDE_SESSION_ID}`、
`user-invocable`/`disable-model-invocation` 开关(最小版两面都可,按需加)。**plan 28 核心生态位
(模型自选 + 渐进披露 + inline/fork + 工具限制)已完整。**
