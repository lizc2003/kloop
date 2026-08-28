# Plan 105 — 会话与 offload 迁到全局的按项目分区存储

## 起因(dogfood)

用户问 `.kloop/sessions` 与 `~/.kloop/sessions` 的关系,结论是**两者毫无关系**:
`main.rs` 把 `sessions_dir` 硬编码成相对路径 `.kloop/sessions`,相对**进程 cwd**,没有
任何 home 兜底。于是同一台机器上的会话散在每个跑过 kloop 的目录里,
`~/.kloop/sessions` 只是"某次进程 cwd 恰好是 $HOME"(Tauri app 以 cwd=$HOME 起
`kloop app-server`)的产物。用户的话:**"我想要找全部的 session 历史,就没法找了"**。

散出去之后连事后归拢都做不到:CLI/TUI 的 rollout 首行是 `provider_route_initial`,
**不记 cwd**(只有原生协议 `thread/start` 才写 `runtime.cwd`),所以文件内部无项目归属,
只能靠它躺在哪个目录反推。

两个参考实现都不这么做:cc 是 `~/.claude/projects/<cwd 路径 slug>/*.jsonl`,
codex 是 `~/.codex/sessions/<日期>/`。kloop 是唯一把会话散在 cwd 里的。

用户拍板:**实现,不用考虑兼容性**(不写迁移/双读代码)。

## 形态

复用 plan 63 已经落地的项目分区(`~/.kloop/projects/v1/<ProjectId>/permissions.json`),
会话与 offload 进同一个桶:

```
~/.kloop/projects/v1/<project-id>/
    permissions.json     ← plan 63,不动
    project.json         ← 新增:{"version":1,"projectId":…,"anchor":"/abs/path"}
    sessions/*.jsonl     ← 原 <cwd>/.kloop/sessions
    offload/{off-NNNN.txt,bg-N.out}
```

为什么用 `ProjectId` 而不是 cc 的路径 slug:`ProjectId` 由 **git common dir** 派生,
主仓与它的所有 linked worktree 天然归一个桶(路径 slug 做不到);而且它是现成的、
已被 permissions 用了的稳定标识,不必新发明一套路径转义/碰撞规则。代价是目录名不可读,
用 `project.json` 里的 `anchor` 补回来 —— `--list-sessions --all` 打的是真实路径,
用户永远不用读 hash。

## 决定

1. **`--mock` 保持 hermetic**:mock 不解析 HOME(既有约束),继续用 cwd 局部的
   `<cwd>/.kloop/{sessions,offload}` 且**不分桶**。否则 mock demo 会往用户真实项目桶里
   写垃圾会话。`SessionStore` 因此是两变体:`Global{root=~/.kloop}` / `Hermetic{root=<cwd>/.kloop}`。
   Hermetic 变体**完全不探测 git**,hermetic 不破。
2. **project id 必须永远有值**:`WorkspaceIdentity::project_id()` 在 git 探测失败时是
   `None`(仓库内容不得成为授权来源)。但转录总得落地,所以新增
   `session_partition()`:有 git 身份就用它,否则退化到 directory 域 hash(cwd)。
   授权语义不变(policy 仍然 disabled),只是存储位置有兜底。
3. **目录必须 0700 创建**:`PrivateDir::open` 会对 `~/.kloop/projects/v1/<pid>/` 做
   `mode & 0o077 != 0` 校验。如果会话代码先用默认 0755 建出这个桶,plan 63 的
   permissions 读写会直接 bail。`SessionStore::ensure` 逐级 `create_dir` + 仅对**新建的**
   那一级 chmod 0700(已存在的目录不动,免得改坏用户的 `<cwd>/.kloop`)。
4. **`--list-sessions` 默认仍是当前项目**(语义不变),新增 `--all` 跨项目列全,按 anchor
   分组、组内按 mtime。`--all` 只能与 `--list-sessions` 同用。
5. **CLI 的 `--resume/--fork <id>` 严格限当前项目**:跨项目 resume 会让 agent 在错误的
   仓库里干活(CLI 会话不记 cwd)。id 不在本桶时扫其他桶,报错直接告诉用户它属于哪个路径。
   **server 相反,必须跨桶查**:`thread/start {cwd}` 可以落在非 default_cwd 的桶里,而
   `thread/resume {threadId}` 不带 cwd —— 只查 default_cwd 的桶会找不到自己刚建的 thread。
   server 的会话有 `runtime.cwd`,恢复后 cwd 自洽,跨桶安全。
6. **sandbox 后果**:`~/.kloop` 整体在 sandbox 的 deny-read/deny-write 里。会话与 offload
   搬进去之后,**被 sandbox 的 bash 再也读不到自己的转录和 offload 文件**(原来
   `<cwd>/.kloop` 是只读但可读)。这是收紧,接受:`read_offloaded`/`read_file`/`bash_output`
   都是进程内读,不过 sandbox;后台 bash 的 `bg-N.out` 是父进程 open 后传 fd,子进程只
   write 已有 fd,不受路径策略约束。
7. **worktree 零改动**:`Config.offload_dir/sessions_dir` 本来就故意不跟随子 agent 的 cwd;
   现在即使跟随也是同一个桶(worktree 与主仓 git common dir 相同)。config.rs 注释更新。

## 不做

- 不写迁移代码、不双读旧路径(用户明确"不用考虑兼容性")。用户已有的 397 个旧会话由
  用户自行 `mv` 或丢弃。
- 不给 CLI 会话补 `runtime.cwd` 行(独立改动,不在本 plan)。
- `thread/list` 不加 `all` 参数(协议面不动),仍是 default_cwd 所在项目。

## 验收

- `cargo fmt` / `clippy --workspace --all-targets -D warnings` / `cargo test --workspace` 全绿
- 新增单测:布局与分区、hermetic 不探 git、0700 创建后 `PrivateDir::open` 仍成功、
  buckets 枚举、跨桶查找
- `--mock` 端到端仍 hermetic(`mock_hermetic` / `headless_contract` 子进程测试不改而通过)
- README 同步

## ✅ 完成（2026-08-29，提交 SHA 以本条所在提交为准）

按上述形态全部落地,无挂账。

- 新增 `crates/core/src/session_store.rs`:`SessionStore::{Global,Hermetic}` + `SessionDirs`
  + `ProjectBucket`,`dirs()`(纯路径,hermetic 不探 git)/`ensure()`(0700 逐级创建 + 写
  `project.json`)/`buckets()`(枚举分区)。
- `project.rs` 新增 `session_partition()`(git 身份不可用时退化到 directory 域)与
  `partition_anchor()`(display only)。
- `config_from_settings` 改收 `&SessionDirs`,不再自己拼路径;`main.rs` 在解析 user config
  **之前**建 store(`--list-sessions` 的坏配置 fast-path 必须活着),`--mock` 走 hermetic。
- `args.rs`:`--all` 标志、`list_every_project()`、`session_in_this_project()`/`owning_project()`
  (跨项目 id 报出所属路径而不是"不存在")。
- `ServerPaths` 从两个固定目录改成 `store`,新增 `Server::locate_thread()`(默认桶优先、
  再扫全部);`thread/start` 用线程自己的 cwd 分区,fork 留在源分区。
- README 新增 "Where sessions live" 一节 + 文件表;`config.rs` 注释更新。

验证:`cargo fmt --check` / `clippy --workspace --all-targets -D warnings` /
`cargo test --workspace` 全绿(2 项真实 provider 合约 ignored)。新增 `session_store` 7 条
单测(含 0700 与 hermetic 不动 git)与 `args` 跨项目 resume 报错逐字断言。手工验证:
伪 HOME 下 `--list-sessions` 打出分区路径、两个分区的 `--list-sessions --all` 按 anchor
分组且组间按最新 mtime 排序。

**如实边界**:未跑真实 provider、Linux sandbox、Windows;sandbox 对 `~/.kloop` 的
deny-read 已有 `startup::tests::sandbox_denies_private_state_tree_reads_and_writes` 覆盖策略
生成,但"被 sandbox 的 bash 读不到转录"未做端到端实跑。用户既有的旧会话未迁移(按拍板)。

### 收尾追记（同一提交）

用户追问"现在还会在 cwd 下创建 `.kloop` 吗"。核查结果:

- **非 mock 模式:不再创建**。cwd 下 `.kloop` 的剩余消费者全是只读输入(`rules/`、
  `skills/`、`commands/`),kloop 不会创建它们;`--worktree` 用的是 `.claude/worktrees`
  (对齐 cc),不是 `.kloop`;scheduler 本来就在 `~/.kloop/scheduler/`。
- **`--mock`:仍然创建**,`<cwd>/.kloop/{sessions,offload}`,这是 hermetic 的定义,故意的。
- **`program-runs`/`workflow-runs` 自动跟着搬了**:`RunStore::new` 以
  `offload_dir.parent()` 为 base,所以它们现在落在 `~/.kloop/projects/v1/{pid}/` 下,
  与 sessions/offload 同桶。这是耦合的既有行为、方向也正确(同生命周期的会话态),
  但原先没被任何测试锁住 —— 已在 `run_store` 既有测试里补一行断言
  (namespace 目录必须是 `offload` 的同级),并写进 README 的 layout 块。
