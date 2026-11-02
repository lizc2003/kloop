# Plan 247 — git 调用收成一个入口

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已完成（2026-11-02）。行号是 2026-10-28 的读数；开工复核后 5 条全部成立。

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
- **2026-11-02 用户定：不需要考虑兼容性。** 新 key 直接取
  `WorkspaceIdentity::session_partition()`（`p1_…`，28 字符），旧目录不读取、不迁移。

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

## 实现记录（2026-11-02）

- **core `git` 模块**（`crates/core/src/git.rs`）：`command(dir)` 统一加 `-C`、固定
  `LC_ALL=LANG=C`、清掉 12 个仓库选择/配置注入变量；`hermetic_command(dir)` 另置
  `GIT_CONFIG_NOSYSTEM=1`、`GIT_CONFIG_GLOBAL=/dev/null`；tokio 版本由 std 命令转换；
  `branch(dir)` 是唯一的取分支实现。Windows 的 verbatim 路径转换与测试 Git 环境钩子
  （`isolate_test_git_environment`）一并挪进该模块，worktree 只 re-export 旧路径。
- **`WorkspaceIdentity`**：探测改走 `git::hermetic_command`；新增 `git: bool`、
  `git_root()` 与 `scheduler_key()`。`git_root()` 只对探测成功的 worktree 返回
  checkout root，普通目录与不可用身份都返回 `None`，指令链不再用 `.git` 存在性猜根。
  `scheduler_key()` 就是 `session_partition()` 的字符串，仓库与 linked worktree 共享，
  不再自己起 git、不再对 lossy 路径做哈希；Git 不可用时也总有 key。
- **worktree**：`git_command` 改走 `git::tokio_command`；夹具提交改用
  `tokio_hermetic_command`，宿主机的 `commit.gpgSign`/hooks 不再影响测试。
- **CLI**：`context::gather` 接收 identity，`git_info` 用 `git::command` 与
  `git::branch`，删除 `find_git_root`；`build_permissions`、`config_from_settings`、
  `trust::ensure_trusted` 都接收已解析的身份；`SessionStore` 新增
  `dirs_with`/`ensure_with`，旧 `dirs`/`ensure` 保留为单次解析的便利入口。
  `run_front_end` 与 server 的每线程 factory 各只 resolve 一次，identity 放进
  `SessionState` 往下传。
- **TUI**：`git_branch` 删除，直接使用 `kloop_core::git::branch`。
- **测试夹具**：core/CLI/server/parity 里所有直接 `Command::new("git")` 的夹具改为
  统一入口；PTY 定时任务夹具改用 `scheduler_key()`。
- **回归**：命令构造断言 12 个变量均被移除、`-C` 与 locale 正确、hermetic 配置固定；
  `branch` 读取真实仓库与子目录；worktree 与非 Git 目录的 `git_root`；scheduler key
  在仓库/worktree 间一致且等于 project partition；`gather` 用身份根读 AGENTS.md 链并
  注入当前分支；不可用身份仍有 scheduler key。宿主机全局 `commit.gpgSign`/hooks 的
  隔离由 `hermetic_command` 固定配置源保证，三处夹具都已改走它；未再单独用敌意
  `HOME` 复跑。
- `make check` 在外层执行沙箱之外全绿（fmt、clippy、workspace 全量测试，core 1296 项、
  TUI 313 项，本机 parity baseline verified）。Linux 与 Windows 未实测。
