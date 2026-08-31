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

## ✅ 切片 2 完成记录(2026-07-16,提交 b17439f)

会话级 enter/exit + `--worktree` CLI,真 key 双轨验收已过。范围(开工问用户定):CLI
`--worktree` + TUI/plain 的 enter/exit,**server 挂账**(每 thread 独立 config、审批不跨
thread、客户端要感知 cwd 切换,需单独设计)。

**核心:运行时可变 cwd(对切片 1"Config 不可变字段"的扩展)**。切片 1 子 agent 的 cwd 是
spawn 前定死的字段;会话级 enter/exit 要**中途立即切换**(对齐 cc `setCwd`/codex
`EnvironmentSwitcher`,一 thread 一棵)。落点:
- `Config.active_worktree: Arc<RwLock<Option<ActiveWorktree>>>`(可变槽)+ `worktree_enabled`
  开关;`effective_cwd/permissions/sandbox/system()` 收口——槽有则用 worktree 的,否则回
  退字段。工具 IO(bash/fs/search/dispatch 权限/sampling system)全改读 effective_*,故
  enter 立即生效。**主 agent 无槽时 effective==字段,存量逐字节不变**。
- `worktree::compute_overrides`(切片 1 的 rewire 4 样抽出)+ `enter/exit/finish_active`
  操作槽;`Worktree::remove` 抽出供 finish/discard 复用。
- `enter_worktree {name}`/`exit_worktree {discard_changes?}` 工具(`tools/worktree_tool.rs`,
  depth-0 + `worktree_enabled` 才注册 + 才允许调;name 消毒 `[A-Za-z0-9._-]` 无 `..`;一次
  一棵已入报错;进 is_readonly 自动放行,不进 is_concurrency_safe 保串行)。exit 无参=脏留
  净删(切片 1 铁律),`discard_changes` 强删。
- `--worktree[=<name>]`(缺省名 `session`,`=` 绑定避开 headless 位置 prompt);启动 enter、
  会话结束 finish_active(脏留 note 打 stderr);**三前端都挂**(tui::run/plain_main/headless
  各一处 enter+finish);与 `--serve`/`--mock` 互斥报错。
- 子 agent 继承:`clone_for_subagent` 用 `effective_*` 定格主 agent 当前有效状态为子 base,
  并给子**全新空槽**(子 agent 不能 enter/exit,不能 alias 父槽 Arc)。

**踩坑**:① headless 是**第三条会话路径**(tui/plain 之外),初版只在 tui/plain 挂 enter/
finish,`--worktree -p` 写穿主仓——真 key 抓到,补 headless 分支(教训:会话级副作用要挂
**所有**前端路径)。② `finish` 的 note 切片 1 写死 "This sub-agent left changes",会话复用后
措辞不符,改中性 "Changes were left in the worktree"。

真 key 验收(headless `-p`,bypass):Anthropic(sonnet-4-6)`--worktree=feat` 启动进树、模型
相对写 out.txt 落 worktree/主仓无、结束脏留 note;模型自调 `enter_worktree→write_file→
exit_worktree` 三步,probe.txt 落 worktree、分支 exp 保留。OpenAI(gpt-5.4-mini)`--worktree`
CLI 同样落树无泄漏。

**切片 2 挂账**:server 会话 worktree;origin/HEAD base;30 天陈旧清理;指令文件/git 快照按
worktree 重组;cc setup 拷贝;跨仓库。

## ✅ server 支持收尾(2026-07-16,提交 72a64ac)

切片 2 挂的 server 会话 worktree 补齐(用户后续要求)。server 每 thread 独立 Config(工厂
闭包造)+ 独立 active_worktree 槽,天然隔离;要补的三点:
- **worktree_enabled 对 server 开**:`config_from_env` 从 `!serve && !mock` 改 `!mock`
  (server 也启用,只 mock 关);enter/exit 工具在 server thread 注册。
- **cwd 切换通知(客户端感知,当初挂账的核心理由)**:`Ui` trait 加 `cwd_changed(cwd,
  branch)` 默认方法(默认退化 note,本地前端也显示);`ThreadUi` 覆盖发结构化
  `thread/worktree` 通知 `{cwd, branch, active}`;enter/exit 工具在切换后调 `ctx.ui.
  cwd_changed`(enter 读槽拿 branch、exit 报回 `cfg.cwd` + null)。`ActiveWorktree` 加 pub
  `branch`。
- **生命周期清理**:`thread_worker` turn 循环结束(server 关闭/turn 通道断)后
  `worktree::finish_active(&cfg)`——模型没 exit 的残留树脏留净删,不泄漏过会话。

`--worktree` 启动 flag 仍单会话(与 `--serve` 互斥保留;server 多会话没有"启动进哪棵"的语
义,worktree 由模型 `enter_worktree` 按需触发)。同名并发:两 thread 同 name enter 撞,
`WORKTREE_LOCK` 串行下第二个报错(模型/客户端该给不同名)。

真 key 验收(Python 驱动 JSON-RPC client 跑真 `--serve`,anthropic sonnet-4-6):模型在
server 会话 `enter_worktree→write_file→exit_worktree`,客户端收到两条 `thread/worktree`
(active:true branch=kloop/worktree/srv → active:false branch=null)、写落 worktree、主仓
无泄漏、exit 脏留分支;server 契约(mock)另测通知形态 + 未 exit 树 shutdown 清理。

**仍挂账**:origin/HEAD base;30 天陈旧清理;指令文件/git 快照按 worktree 重组;cc setup
拷贝;跨仓库;server 同名 worktree 的客户端协调。

## 修正（2026-08-29 → 2026-08-31，plan 105 会话中 dogfood 发现）

`WORKTREES_DIR` 原本是 `.claude/worktrees` —— 用户看到后一句「这个不合理」。确实:
本文件上面写的收敛点是「**仓内专用目录**」,codex 用 `.codex/worktrees`、cc 用
`.claude/worktrees`,两家都用**自己的**命名空间;kloop 照抄了 cc 的**字面目录名**,
等于把自己的工作树写进另一个产品的目录里(README 里甚至有一句「kloop scans only its
own `.kloop/`, not cc's `.claude/`」,自相矛盾)。同一仓库里同时用 cc 和 kloop 时,
两边的 managed 树与分支还会挤在一个命名空间里。分支前缀同理,`worktree-<slug>` 和 cc
逐字相同,一并改掉(最终形态见下一节)。

**这其实是一次回退,不是新决定。** plan 35 原本用的就是 `.kloop/worktrees` +
`kloop/worktree/*`(本文件上面 92/181 行仍是当时的记录);是 **Plan 56「Worktree 工具对齐」**
在追 Claude Code 2.1.220 的 exact parity 时,把受管目录和分支前缀一并换成了 cc 的字面值
(`56-worktree-parity.md:47`)。对齐该抄的是**行为契约**——enter/exit 的 schema、custody
与删除权、清理判定;**命名空间**抄过去就变成了把自己的状态写进别人家。

目录名反复过一次,过程值得留下:

**第一版落在 `.kloop-worktrees`(同级兄弟)**,理由是别往 fail-closed 的分类器上打洞。
**用户拍板「我看重 `.kloop/` 整齐」,改为 `.kloop/worktrees`**,代价是三个豁免。

回源核对(第一版说「cc/codex 显然没有这层保护」是错的,两家都有):

- **cc 有一模一样的规则,并且为此开了特例**:`DANGEROUS_DIRECTORIES` 含 `.claude`,
  `isDangerousFilePathToAutoEdit`(`filesystem.ts:447`)逐 segment 扫,和 kloop 的
  `path_is_sensitive` 同形;它硬编码了「`.claude` 紧跟 `worktrees` 就跳过」,注释写明
  这是 structural path,并且跳过后继续扫剩余 segment,树**内部**的 `.claude/` 照拦。
- **codex 不需要特例,因为规则窄一档**:`default_read_only_subpaths_for_writable_root`
  (`protocol/src/permissions.rs:1632`)保护的是 `writable_root.join(".codex")`——
  每个 writable root 的**顶层**,不是路径里任何一处 component。进树后 writable root
  换成树本身,`<repo>/.codex/worktrees/...` 从不在判定范围内。

kloop 的规则是 cc 那种形状,所以走 cc 那条路:一个豁免,三处接线。

**三处豁免(缺一不可,且都 fail-closed 守 `..`)**:

1. `path_is_sensitive`:逐 component 扫时,`.kloop` 紧跟 `worktrees` 则 `continue`
   (不是 return false——继续扫,树内嵌套的 `.kloop` 仍然敏感)。
2. `raw_mentions_sensitive_path`:裸串层。**必需**,不是优化——它命中就是**硬 deny、
   不可审批**(`permissions.rs:828`),而 `bash_reads_sensitive_path` 会把相对路径按 cwd
   归一化,cwd 就是工作树,所以不开这个洞的话树里**每一次读**都被拒。
3. `powershell_mentions_sensitive_path`:同上。

裸串两层(2/3)只在命令里**不含 `..`** 时才给豁免——argv 解析失败时它们是仅剩的防线,
`.kloop/worktrees/../sessions` 不能从洞里走出去。`path_is_sensitive` 也同样守:
路径里出现任何 `ParentDir` 就整体作废豁免,不依赖调用方先归一化(这条是被自己的测试
抓出来的——第一版只跳 segment,`..` 直接穿过去了)。

**sandbox 不需要豁免**,这点第一版判断也错了。`protected_subpaths` 把每个 writable
root 的 `.kloop` 设为只读子路径,但 SBPL 生成的是**一个 `(allow file-write* 部分0 部分1 …)`
的析取**:进树后 `for_workspace` 把工作树本身加成 writable root,它那一部分独立放行,
仓库根那一部分的 `require-not` 只约束它自己。即使用户额外把仓库根配成 writable root
也不受影响。已加测试 `a_worktree_under_the_protected_state_dir_stays_writable` 钉死。

### 分支前缀:为什么是扁平的 `kloop-worktree-` 而不是 `kloop/worktree/`

第一版跟 codex 取了 slashed 的 `kloop/worktree/<name>`。用户问「用 `/` 做分割好吗」,实测两向:

```
$ git branch kloop && git worktree add -b kloop/worktree/x wt1
fatal: cannot lock ref 'refs/heads/kloop/worktree/x':
       'refs/heads/kloop' exists; cannot create 'refs/heads/kloop/worktree/x'

$ git worktree add -b kloop/worktree/x wt1 && git branch kloop
fatal: cannot lock ref 'refs/heads/kloop':
       'refs/heads/kloop/worktree/x' exists; cannot create 'refs/heads/kloop'
```

第二向是真代价:**只要有一棵托管树活着,用户就建不了叫 `kloop` 的分支**——ref 存成
`refs/heads/` 下的文件,slashed 前缀要求 `kloop` 是目录,两者互斥。而在 kloop 自己的仓库里
`kloop` 恰是除 `main` 外最可能被用到的名字。换来的只有 `git branch` 里的分组显示。

这正是 `encode_name` 把名字里的 `/` 编码成 `+` 时已经避开的同一个 D/F 冲突,从前缀那头请
回来不划算;cc 选扁平 `worktree-` 时的注释写的也是这条。codex 用 slashed 能跑,是因为随便
哪个仓库里叫 `codex` 的分支不太可能存在——那是运气,不是设计没问题。

扁平的 `kloop-worktree-<name>` 冲突面归零(`kloop-worktree` 与 `kloop-worktree-x` 都是
文件,不互斥),`git branch --list 'kloop-worktree-*'` 照样筛。目录侧保持 `.kloop/worktrees/`
——那边是真目录,天然分层,没有这个问题。
