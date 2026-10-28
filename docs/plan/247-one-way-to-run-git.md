# Plan 247 — git 调用收成一个入口

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量;标「待复核」的条目开工第一步
> 先读代码确认，不成立就在本文件划掉并写明原因。

## 问题

1. **环境加固只做在一处**(已复核)。`core/src/project.rs:151` `git_probe_command` 清掉
   `GIT_IDENTITY_ENV_VARS`、设 `LC_ALL=C` 与空全局配置;其余 git 调用直接继承进程环境：
   - `core/src/worktree.rs:874` `git_command`(生产路径只加 `-C`);
   - `cli/src/startup.rs:961` `scheduler_project_identity`;
   - `cli/src/context.rs:348` `git_info`(分支、status、log 注入给模型);
   - `tui/src/lib.rs:363` `git_branch`。
   kloop 从 git hook、`git rebase -x` 里启动时带着 `GIT_DIR`/`GIT_WORK_TREE`/
   `GIT_INDEX_FILE`:身份按 cwd 真实结构算，worktree 创建/删除、scheduler key、注入的
   分支与 status 却指向另一个仓库;`GIT_INDEX_FILE` 指向主仓库 index 时，在 worktree 里
   跑 status/checkout 会改写主仓库的 index。
2. **scheduler 另算了一套项目身份**(待复核)。`cli/src/startup.rs:961-990`:
   - git 失败时 `?` 上抛，整个启动失败(git < 2.31 不认 `--path-format=absolute` 会原样
     回显，`canonicalize` 失败——这条未实测);
   - 对 `to_string_lossy()` 做哈希，而 `WorkspaceIdentity`(`project.rs:187`、`:348`
     `hash_id`)对 OsStr 原始字节加 domain 前缀;非 UTF-8 路径会撞 key;
   - 同一个 `startup.rs` 在 `:453` 已经 resolve 过 `WorkspaceIdentity`。
3. **同一件事写了多份**:
   - 查 common dir:`project.rs:151`、`startup.rs:961`、`worktree.rs:806`;
   - 取分支名：`tui/src/lib.rs:363` 与 `cli/src/context.rs:357`(`trim` 与 `trim_end` 之差);
   - 找仓库根：`cli/src/context.rs:340` `find_git_root`(`.exists()`,悬空 `.git` 链接算没有)
     与 `project.rs:174` `git_marker_root`(`symlink_metadata`,出错算有),边界情况下
     AGENTS.md 链的根与 `workspace_root` 不一致。
4. **一次启动对同一个 cwd 起 6–7 个 git 进程**:`WorkspaceIdentity::resolve` 被调 3 次
   (`cli/src/trust.rs:61`、`SessionStore::ensure`、`build_permissions`),scheduler 1 次，
   `git_info` 3 条命令。
5. **测试 git 夹具隔离不一**:`tools/mod.rs:2237`、`cli/src/startup.rs:1375`、
   `tools/subagent.rs:1434` 直接 commit,继承宿主机全局配置(`commit.gpgSign`、hooks);
   `session_store.rs:305`、`cli/src/project_store.rs:383` 只设了 `NOSYSTEM`。教训 272 记过
   同一类失败。

## 开工先问用户

- scheduler key 已经落盘。改由 `WorkspaceIdentity` 派生后，旧 key 下的持久任务怎么办：
  (a) 读取时兼容旧 key(两个 key 都查，写入只用新 key);(b) 启动时一次性迁移。推荐 (a),
  不改用户文件。

## 做法

1. core 新增 `git` 模块(或放进 `project.rs` 旁):`git::command(dir) -> Command`,统一
   `-C`、`env_remove(GIT_IDENTITY_ENV_VARS)`、`LC_ALL=C`。是否屏蔽全局配置由调用方选：
   身份探测屏蔽(现状),worktree/status 不屏蔽(要尊重用户的 git 配置)。worktree 的
   `TEST_GIT_ENVIRONMENTS` 测试钩子挪进这个入口。
2. worktree、cli 的 `git_info`/scheduler、tui 的 `git_branch` 全部改用它;tui/cli 可以
   直接调 core 的公开函数，不再各自起进程。
3. scheduler key 从 `WorkspaceIdentity`(`partition_anchor()` 或 `project_id`)派生，按
   问答兼容旧 key;git 失败不再让启动失败。
4. AGENTS.md 链的根改用 `WorkspaceIdentity::workspace_root()`,删 `find_git_root`;分支名
   做成 core 的一个函数。
5. 每个 cwd 只 resolve 一次 identity,放进 `SessionState` 往下传。
6. 测试 git 夹具统一走第 1 步的隔离环境(关签名、`core.hooksPath` 指空目录、全局配置置空)。

## 验收

- 单测/集成(旧代码先红):导出 `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` 指向另一个
  仓库时，worktree 创建/删除、`git_info`、TUI 分支、scheduler key 都按 cwd 的仓库算;
  scheduler key 与 `WorkspaceIdentity` 一致且旧 key 的任务仍能读到;git 不可用时启动不失败。
- 宿主机全局配置开 `commit.gpgSign` 时上述三处测试仍过(照教训 272 的写法)。
- DESIGN 补"git 调用统一入口"一句(先读 project 一节现在怎么写);外层执行沙箱之外
  `make check`;HANDOFF 记进展与教训。
