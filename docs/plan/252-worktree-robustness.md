# Plan 252 — worktree 健壮性

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。git 命令的统一入口在 plan 247,
> 前台中断时的 worktree 泄漏在 plan 249,这里不重复。

## 问题

1. **任一失效的已登记 worktree 让所有 worktree 操作失败**(已复核)。
   `core/src/worktree.rs:810-830` `registered_worktrees` 对 `git worktree list --porcelain`
   的每个条目 `std::fs::canonicalize(..)?`。仓库里某个无关 worktree 目录被手动 `rm -rf`
   (git 仍列成 prunable)或在没挂载的盘上，`discover_repository`(create/enter/enter_existing、
   `run_agent {isolation: worktree}`)与 `verify_provenance`(finish、exit remove)全部报
   "canonicalizing registered worktree …"。
2. **`enter_worktree` 的相对 `path` 按进程 cwd 解析**(待复核)。
   `core/src/tools/worktree_tool.rs:148` + `worktree.rs:490`:`PathBuf::from(path)` 直接
   `canonicalize`,相对的是进程 cwd 而不是 `cfg.effective_cwd()`。server 模式各线程 cwd 不同、
   或已进入某 worktree 后用 `../x` 切换时，报 "not a registered worktree" 或进入与审批时看到的
   不同目录。其他工具的相对路径都按 effective cwd 解析。
3. **provenance 校验重复**(优化)。`remove_for_session`/`finish` 先 `verify_provenance`,紧接着
   `remove_managed_locked` 又 verify 一次：每次多 2 个 git 进程和一次 canonicalize。
4. **async 路径里的阻塞 IO**(优化)。`worktree.rs:316`、`399-402`、`490`、`768`、`849`:
   `create_managed`、`enter_existing`、`verify_provenance`、`registered_worktrees` 在 async fn
   里直接 `canonicalize`/`create_dir_all`;`exclude_worktrees_dir` 持有 async 的
   `WORKTREE_MUTATION_LOCK` 时做阻塞式 `StateLock::acquire`(flock);`compute_overrides`
   同步调两次 `WorkspaceIdentity::resolve`(阻塞 git 子进程)。
5. **构建过的任务 worktree 一律保留**(观察)。`inspect_changes` 用 `--ignored=matching`,
   被 gitignore 的产物也算改动(有测试 `ignored_files_are_content_changes` 为证，是刻意的),
   副作用是跑过 `cargo build`/`npm install` 的 worktree 在 `finish` 时总被保留，大目录越积越多。

## 开工先问用户

- 第 5 条要不要改?候选：(a) 维持现状，只在 DESIGN 写明取舍;(b) 只有 ignored 改动、没有
  tracked/untracked 改动时，删除并在 Note 里列出被丢弃的 ignored 目录。推荐 (a) 先写明，
  (b) 等有真实抱怨再做——ignored 文件里也可能有用户在意的东西(`.env.local`)。

## 做法

1. canonicalize 失败的条目跳过(或保留原始路径并标记 prunable),不中断整个列表;用到该条目
   的操作给出明确错误。
2. `enter_worktree` 在 canonicalize 前先 `ctx.cfg.effective_cwd().join(path)`。
3. `remove_managed_locked` 增加"调用方已校验"的内部变体。
4. 阻塞调用改 `spawn_blocking` 或 `tokio::fs`;flock 改在持锁前获取或放进 `spawn_blocking`;
   `compute_overrides` 复用已有的 identity。
5. 按问答处理第 5 条。

## 验收

- 单测(旧代码先红):删掉一个已登记 worktree 目录后，enter/create/finish 仍可用;相对 path
  在 effective cwd 与进程 cwd 不同时进入正确目录;finish 路径 git 进程数减少(可用测试钩子计数)。
- 测试 git 夹具照 plan 247 的隔离写法;外层执行沙箱之外 `make check`;DESIGN 的 worktree 一节
  先读再改;HANDOFF 记进展与教训。
