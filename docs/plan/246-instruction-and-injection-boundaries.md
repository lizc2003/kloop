# Plan 246 — 指令文件与注入的信任边界

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 2026-11-02。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

1. **项目指令文件可经符号链接读到项目外**(已复核)。`cli/src/context.rs:140-225`:
   - 顶层 `AGENTS.md`、`AGENTS.local.md` 与 `.kloop/rules/*.md` 完全不查边界;
   - `@import` 只用 `lexical_normalize` 的结果判 `is_external`,读取时却
     `canonicalize` 跟随符号链接(`expand()` 里先 `canonicalize` 再 `read_to_string`)。
   - 恶意仓库放 `AGENTS.md -> ~/.kloop/config.toml` 或 `.kloop/rules/x.md -> ~/.ssh/id_rsa`,
     内容进 `<project-instructions>`,每次请求都发出去;这次读取在 kloop 进程内，绕过
     沙箱对 private state root 的拒读。headless/serve 不弹信任提问，直接生效。
   - DESIGN 约 2279 行："Project/local files may only import from within the project"。
2. **skill 的 `arguments` 先拼进正文再扫描注入**(已复核)。`core/src/tools/skill.rs:59-64`:
   `expand_body(&skill.body, &skill.dir, args)` 之后才 `inject::expand`。正文没有
   `$ARGUMENTS` 时末尾追加 `ARGUMENTS: <args>`,于是模型可在参数里写 `` !`cmd` `` 或
   `@/path` 自己触发注入。`!cmd` 走 `run_gated_bash`(权限门与审批仍生效),但**不跑
   `pre_tool` hook**;`@file` 只查 `read_path_blocked`,用户配置的 `ask = ["read_file(...)"]`
   不生效。`inject.rs:69-70` 的注释已经防"命令输出驱动文件读取",说明"不可信内容不应
   驱动注入"是设计意图。
3. **skill 路径里对注入 `!cmd` 答 No 不结束 turn**(待复核)。`core/src/tools/inject.rs:181-185`:
   `check_call_with_execution` 把 `Refusal::Stopped` 映射成普通拒绝字符串，`ctx.stop`
   不 cancel;对比 `tools/mod.rs:1294-1297`。另外 `Ok(Some(notice))`(如"项目规则未保存")
   被 `?` 丢掉。slash 路径在 turn 外，语义可保留。
4. **`@file` 先整份读进内存再截 100KB**(待复核)。`inject.rs:212-227`:同步
   `read_to_string`,几 GB 的日志会读满再丢弃并卡住 tokio 线程;100KB 之后出现非 UTF-8
   时整个文件被静默跳过。
5. **`skill_tool` 不查 depth**(待复核，观察级)。`core/src/tools/builtin.rs:375-377` 说
   `Builtin::Skill` 由 owner 自己复查，但 `skill_tool` 既不查 depth 也不查注册状态，只有
   `fork_skill` 在 depth≥1 时降级成 inline。

## 开工先问用户

- 第 1 条：项目内的符号链接指向项目外时，(a) 一律跳过并给一条警告;(b) 允许，但排除
  private state root 与凭据目录。monorepo 里 `AGENTS.md` 链到仓库外共享文件的用法会被
  (a) 挡掉。推荐 (a),与 DESIGN 现有措辞一致;要支持共享文件可走全局 scope。
- 第 2 条：除了"注入只扫作者写的正文",skill 路径里的 `!cmd` 要不要也跑 `pre_tool` hook?
  推荐要：skill 路径在 turn 内，hook 是 bypass 模式下唯一的自动化策略。

## 做法

1. `expand()` 对 Project/Local scope 一律检查 `canon.starts_with(canonical boundary)`
   (顶层文件、rules、import 同一处检查);越界按问答处理。boundary 本身也取 canonical。
2. 注入扫描移到参数替换之前，只扫 `skill.body`;替换进来的参数片段不再参与注入。
   按问答给 skill 路径的 `!cmd` 加 `pre_tool` hook。
3. skill 路径改用区分 `Refusal` 的检查,`Stopped` 时 `ctx.stop.cancel()`,notice 经
   `ctx.ui.emit` 发出;slash 路径保持现状。
4. `@file` 改为 `File::open(..).take(MAX_ATTACH_BYTES + 4)` 有界读取，截到字符边界或
   lossy;整段放进 `spawn_blocking`。
5. `skill_tool` 开头补 depth 检查(或在 `unoffered` 覆盖)。

## 验收

- 单测(旧代码先红):顶层 `AGENTS.md`、rules 下文件、`@./link.md` 三种符号链接指向
  项目外均不被读入并有警告;项目内互链照常;`skill{arguments:"!`echo x`"}` 与
  `arguments:"@/etc/hosts"` 不触发注入;skill 注入 `!cmd` 答 No 结束 turn 并有
  notice;`@file` 读大文件只读前 100KB、后段非 UTF-8 不影响前段;depth≥1 调用 skill 被拒。
- DESIGN 改写约 2279 行指令文件边界与约 1195 行注入一节;外层执行沙箱之外 `make check`;
  HANDOFF 记进展与教训。

## 实施记录

- 第 1 条按用户选 (a)：`Discovery::expand` 在 `canonicalize` 之后统一检查 Project/Local
  scope 是否落在 canonical git root（无 git root 时 cwd）内，顶层 `AGENTS.md`、
  `AGENTS.local.md`、`.kloop/rules/*.md` 和 `@import` 走同一处；越界不再读取，记
  `instruction file outside the project, skipped: <spelling>`。原先按词法路径判断 import 的
  `is_external` 随之删除，避免保留一条会被符号链接绕过的旧检查。Global scope 不受限。
- 第 2 条按用户同意给 skill 路径的 `!cmd` 补 `pre_tool` hook。`inject.rs` 新增
  `expand_skill_body`：先在作者正文上找 `!cmd`/`@file` 标记，再分段替换
  `$ARGUMENTS`/`$N`/`${CLAUDE_SKILL_DIR}`；命令自身文本先替换（`!`cmd $ARGUMENTS`` 仍可用），
  参数替换进来的文本与末尾 `ARGUMENTS:` 行都不再参与扫描。`skills.rs` 抽出
  `substitute` 供分段调用，`expand_body` 行为不变。slash 路径顺序与语义保持原样。
- 第 3 条：skill 路径改走 `check_call_with_resolved_path` 并区分 `Refusal`；`Stopped` 时
  `ctx.stop.cancel()` 并返回 `user_stop("bash")`，`Ok(Some(notice))` 经 `ctx.ui.emit` 发出。
  slash 路径仍用 `check_call_with_execution`，错误文本不变，因为它不在 turn 内、没有可取消
  的 turn。
- 第 4 条：`@file` 由 `File::open(..).take(MAX_ATTACH_BYTES + 4)` 有界读取，整段放进
  `spawn_blocking`；前缀按 UTF-8 解码，截断落在多字节字符中间时截到前一个字符边界，
  100KB 之后才出现的非 UTF-8 字节不再导致整份文件被跳过；cap 内的非法 UTF-8 仍按二进制
  跳过。
- 第 5 条：`skill_tool` 开头检查 `ctx.depth > 0`，用 `Unoffered::RootOnly` 的同一句
  `tool 'skill' is only available to the root agent` 拒绝；`unoffered` 保持把 `Elsewhere`
  交给 owner 复查的设计。
- 验证（2026-11-02）：`cli/src/context.rs` 新增顶层 `AGENTS.md`、rules、`@./link.md` 三种
  越界符号链接与项目内互链测试；`tools/skill.rs` 新增参数注入两条、命令内 `$ARGUMENTS`、
  `pre_tool` 阻断、Stop 结束 turn、depth≥1 拒绝；`tools/inject.rs` 新增大文件截断/后段二进制、
  notice 与 Stop 语义。先按旧代码意图断言（参数里的标记展开、Stop 只当普通拒绝），改后全绿。
  外层执行沙箱之外 `make check` 全绿（fmt、clippy、workspace 全量测试，本机 parity baseline
  verified）。Windows 未实测。
