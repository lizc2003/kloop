# Plan 35 — worktree 隔离(并行子 agent 的工作区隔离)

> 一句话定位:kloop 的并行/异步子 agent 已建齐(plan 17/26),但多个子 agent 并行**写**
> 同一工作区会互踩——现在只敢派只读活(教训 18:生态位没打开,能力就不会被用)。git
> worktree 隔离是打开"并行改代码"的钥匙。cc 与 codex 双家强收敛。

## 回源结论(2026-07-15,两家真读,file:line)

- **codex**(`ext/worktree/` 整 crate):三类 owner `ManualTool/Subagent/Cli`
  (`service.rs:42-54`);目录 `.codex/worktrees/<name>` + 分支
  `codex/worktree/<name>`(`service.rs:28-31/1118-1132`),创建 = `git worktree add
  --no-track -B <branch> <path> <base>`(:191-200),managed 目录注入
  `.git/info/exclude`(:934-957)。入口两个:模型工具 `enter_worktree`/`exit_worktree`
  (`tool.rs:39-40`,一 thread 一棵)+ 多 agent `spawn_agent` 的
  `workspace_isolation: shared|worktree` 参数(`multi_agents_v2/spawn.rs:315-323`,子
  config 的 cwd/workspace_roots 重定向 :437-467)。生命周期:**未变更(clean 且无
  base 之上 commit)自动删;dirty/unmerged 一律保留;没有任何自动 merge/PR 路径**
  (`cleanup.rs:120-137`);30 天陈旧清理只删匿名 owner 的干净树、ManualTool 永远保留
  (`service.rs:667/736-777`);默认 `enabled=false`(`config.rs:34`)。
- **claude-code**:目录 `.claude/worktrees/<slug>` + 分支 `worktree-<slug>`
  (`utils/worktree.ts:204-227`)。同样两个入口:模型工具 `EnterWorktree`/
  `ExitWorktree`(builtin-tools,创建隔离树并把会话切进去)+ agent 级
  `createAgentWorktree`(:902-952,workflow `isolation:'worktree'` →
  `claudeCodeBackend.ts:219-235`,**fail-closed:创建失败 agent 直接死,不回退共享
  cwd**)。清理:无变更自动删、有变更保留(`hasWorktreeChanges` :1146-1175,git 出错
  也判 changed——fail-closed);周期清理只碰 ephemeral slug、跳过 dirty/未推送
  (:1060-1138);**同样没有自动合回**。
- **收敛点(形状照抄)**:① 仓内专用目录 + 每树一分支;② 双入口——子 agent 隔离参数
  与模型工具 enter/exit(可分片);③ 生命周期:**未变更自动删、有变更保留、绝不自动
  合**(变更留在分支上给用户处置);④ 清理判定 fail-closed(拿不准就当有变更);⑤ 陈
  旧清理只碰匿名/子 agent 树。
- **分歧(不必抄)**:base ref(codex 默认 origin/HEAD 先 fetch,cc 默认 origin 默
  认分支;kloop 单机开发 HEAD 起步最便宜);cc 的 setup 拷贝(settings/hooksPath/
  symlink/`.worktreeinclude`)是它产品面的包袱。

## kloop 现状与落点

- 切片 1(核心):`task` 加 `isolation` 参数(`"worktree"` 时 spawn 前建树),子
  Config 的 cwd 换成 worktree 路径。**kloop 的 cwd 是权限/沙箱/上下文的锚点**
  (acceptEdits 判 cwd 内、sandbox writable root、指令文件发现、git 快照)——换 cwd
  会自然联动,这既是红利也是坑:开工时对着 Config 逐字段过一遍哪些该跟、哪些不该
  (如指令文件按 worktree 内容重新发现应是对的,git 快照亦然)。
- 子 agent 终态:未变更 → 删树删分支;有变更 → 保留,**回灌摘要/tool_result 里带
  worktree 路径 + 分支名**(父或用户手动合,对齐两家"不自动合")。
- 创建失败 fail-closed(cc 形):task 报错,不静默回退共享 cwd。
- 非 git 仓库:参数被显式请求时报错(不静默降级)。`.kloop/worktrees` 注入
  `.git/info/exclude`(codex 形)。

## 关键决定(开工时定 / 问用户)

1. **入口首片**:task 参数(倾向,机器面先行)vs 模型工具 enter/exit(切片 2 挂账)
   vs agent_type 配置里声明。
2. **base ref**:HEAD(倾向,本地便宜)vs origin/HEAD(要 fetch,CI 场景再说)。
3. **cwd 联动清单**:权限 cwd 相对匹配 / sandbox writable_roots / 指令文件 / git 快照
   / `.kloop` 敏感路径判定(worktree 里的 `.kloop` 也该敏感)逐项定;offload 目录与
   sessions 目录**不跟**(留主仓,子会话落盘照旧)。
4. **同名复用**:同名树存在时报错(倾向,子 agent 名自动生成不会撞)vs 复用(cc 会话
   级语义,挂账)。

## 不做(挂账)

自动合回/PR(两家都没有,变更留分支);enter/exit 模型工具与 `--worktree` 会话级
CLI(切片 2);30 天陈旧清理(单机先手动 `git worktree prune`,有痛感再抄整段);cc
的 setup 拷贝;跨仓库(worktree 只在当前 git 仓内)。

## 测试

git 临时仓 fixture:创建(路径/分支名/`.git/info/exclude` 注入);未变更子 agent 结束
→ 树与分支被删;有变更 → 保留 + 回灌文本含路径与分支;创建失败(非 git 仓/名撞)报
错不回退;权限:worktree 内 acceptEdits 写放行、主仓路径在子 agent 里不因 cwd 换而误
放;沙箱 writable root 指向 worktree(策略 argv 纯函数断言)。

## 完成标准

fmt/clippy/test 全绿,一次 commit;README 补 isolation 参数;本文件补完成记录;HANDOFF
补能力条目与教训(cwd 联动清单的取舍)。真 key 验收:两个并行子 agent 各自 worktree
改同一文件不冲突,变更分支可手动 merge。

## ✅ 完成记录(2026-07-16,提交 9e7c87f)

**切片 1 落地,真 key 双轨验收已过。** 关键决定按 plan 倾向:入口=task 的 `isolation`
参数、base=HEAD、同名报错、offload/sessions 不跟。

**发现:cwd 尚不是 Config 一级字段**。bash/read_file/write_file/edit_file/grep/glob
全部隐式吃进程 cwd(bash 不 `current_dir`、文件工具直接拿裸 path、grep/glob 默认根
`"."`)。要真隔离必须把 cwd 提成 `Config.cwd` 并穿进每个工具 IO——主 agent cwd=进程
cwd 故行为逐字节不变,只有 worktree 子 agent 分叉。这是 plan 预警"cwd 是锚点"的实体,
比"加个参数"大。落点:
- `Config.cwd`(新字段,CLI 用 `current_dir()` 填);tools 里 `resolve_path(cwd, raw)`
  统一锚定;bash `.current_dir(cwd)`;grep/glob 根默认 cwd + `display_path(root,·)`
  改成根相对(主 agent 输出不变)。
- `core/src/worktree.rs`:`create`(`git worktree add --no-track -B kloop/worktree/<agent-N>
  <dir>/<name> HEAD` + `.git/info/exclude` 注入)/`finish`(未变更→删树删枝;有变更→留 +
  回灌 note 带路径分支)/fail-closed(非 git/名撞/git 错=报错不回退;change 探测出错=当有变更留)。
- `Permissions::rebased(cwd)`(rules/approver/persist 共享、session 缓存重置、cwd 换;
  `PersistFn` Box→Arc 才能共享)、`SandboxPolicy::with_writable_root`(worktree 加进可写根)。
- task `isolation` 参数 + schema `enum[shared,worktree]`;sync 与 background 都覆盖。

**三处真 key 踩坑(mock 用 allow_all 短路全门,全都藏住了)**:
1. **模型用绝对路径逃逸**:子 agent 继承的 `system` 环境块写着**父** cwd,模型据此拼绝对
   路径,写穿 worktree 落回主仓。修:`rewire_for_worktree` 把 system 里 `- Working
   directory:` 那行改成 worktree。(指令文件/git 快照仍继承——HEAD worktree 文件逐字节
   同,重新发现要 CLI IO,挂账。)
2. **worktree 建在 `.kloop/` 下与敏感路径规则冲突**:`.kloop` 是权限门 `path_is_sensitive`
   与沙箱只读子路径双重保护的路径,worktree 里每次写都被判"写 kloop 配置"→安全检查
   bypass 免疫→headless 无 approver 直接拒。改到 `.kloop-worktrees/`(不含 `.kloop`
   组件)一并躲开两处。**偏离 plan 的 `.kloop/worktrees` 位置**,理由充分。
3. **并行 `git worktree add` 竞态**:两个并行子 agent 同仓建树,ref/worktree-admin 锁
   互踩,一个静默丢树。加进程级 `WORKTREE_LOCK`(tokio Mutex)串行化 create/finish 的
   git 变更(读探测不锁)。

**回灌 note 修正**:子 agent 改文件但**不 commit** 时变更是 worktree 工作区里的未提交改
动,`git merge <branch>` 是 no-op。note 改成"改动在 worktree,提交后 merge 或直接看/删"。

真 key 验收(`--permission-mode bypass`,headless `-p`):Anthropic(claude-sonnet-4-6)
两并行子 agent 各在 worktree 改同一 `shared.txt`→主仓无变更、两分支 AGENT-A/AGENT-B
互不冲突、commit 后可 merge 回 main;OpenAI(gpt-5.4-mini)子 agent 相对写 `new.txt`
落 worktree、主仓无泄漏、clean 树自动撤除。

**挂账不变**:enter/exit 模型工具 + `--worktree` 会话级(切片 2);origin/HEAD base;30 天
陈旧清理;指令文件/git 快照按 worktree 重新组装(需 CLI IO);cc 的 setup 拷贝;跨仓库。
