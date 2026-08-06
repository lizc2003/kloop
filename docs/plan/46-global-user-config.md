# Plan 46 — 唯一全局用户配置

## 背景

Plan 40 把 provider/model/credential 放在 `~/.kloop/config.toml`，同时保留 cwd `.kloop/config.toml` 承载 permissions、MCP、web、hooks、sandbox、agents、codemode。这样同一进程的行为会随启动目录或 app-server thread cwd 漂移，仓库文件也能改变执行策略；AllowAlways 与 MCP OAuth 状态还分别写回项目目录，作用域和安全边界不一致。

本计划删除整个项目 `config.toml` 配置层。唯一自动 TOML 配置源改为 `~/.kloop/config.toml`；cwd 只表示工作区，不再参与配置发现。

范围只包括 `<cwd>/.kloop/config.toml`。项目 instructions、`.kloop/rules/`、skills、commands、sessions、offload、program journal、worktree 等 cwd 机制不变。

## 回源结论（2026-07-24）

- Codex/Codex 和 Claude Code 可以加载项目配置，是因为它们同时维护 workspace trust、项目字段过滤与执行门禁；项目配置不是一个可单独复制的便利层。
- Claw Code 的实现展示了 trust 链不闭合时，项目配置可把仓库内容变成执行策略或提权入口。
- kloop 目前没有必须由仓库 TOML 覆盖的部署场景。为一层可变 cwd policy 引入完整 trust 系统，复杂度和风险都高于收益。
- 因此取更小且确定的边界：一个私有用户 TOML 保存持久策略；CLI、app protocol 和环境变量只作单次覆盖；cwd 只锚定工作区状态。

## 决定

1. **唯一自动 TOML**
   - 非 `--mock` 只读取、解析一次 `~/.kloop/config.toml`。
   - 根 schema 允许 provider/model 键，以及 `permissions`、`mcp`、`web`、`hooks`、`sandbox`、`agents`、`codemode`。
   - 各 feature 模块从同一个根表做严格 typed 解析；未知键和错误类型 fail closed，错误不得回显配置原文或 secret。
   - `--mock` 不解析 HOME、TOML 或 provider/runtime 配置环境变量。

2. **cwd 不再是配置源**
   - 不探测、不读取、不解析、不合并，也不为 legacy `<cwd>/.kloop/config.toml` 发运行时警告。
   - TUI、plain、headless、app-server 与所有 server thread 共享同一进程级 runtime snapshot。
   - thread cwd 仍分别锚定 project context、skills/commands、权限路径、sandbox workspace root 和工具 IO。

3. **持久化与 OAuth 全局化**
   - `p` / AllowAlways 写入 `~/.kloop/config.toml` 的 `[permissions].allow`，并明确影响所有工作区。
   - 写回拒绝 symlink、非普通文件、过宽权限和不私有的目录；同目录创建 `0600` 临时文件，写透并原子 rename；保留其他 TOML section 的语义并去重规则，但不承诺保留注释和排版。
   - 保存成功后同步进程内 permission-rule snapshot，使之后创建的 app-server thread 立即继承新规则。
   - MCP OAuth store 改为 `~/.kloop/mcp-oauth.json`，复用相同私有原子读写边界；`kloop mcp login` 只解析全局 MCP section，不要求 provider/model/key 已完整。
   - macOS sandbox 同时 deny-read 全局 config 与 OAuth store；模型工具侧既有 `.kloop` 敏感路径硬门保留。

4. **覆盖层与迁移**
   - provider 既有环境变量优先级不变；`KLOOP_ALLOW`/`DENY`/`ASK`、`KLOOP_SANDBOX`、`KLOOP_PROGRAM_*` 等仍是进程级覆盖。
   - `[web] search_provider = "tavily" | "brave"` 位于全局 TOML；搜索 key 仍只走 `TAVILY_API_KEY` / `BRAVE_API_KEY`。
   - 不新增任意 `--config PATH`，避免重新引入不受控配置来源。

## 实现

- 新增 `cli/src/user_config.rs`，集中负责路径发现、一次 TOML 读取、根 schema、私有读取和原子写回。
- `provider_config.rs`、`mcp.rs`、`web.rs` 改为消费已解析根表，不再自行读文件。
- `startup.rs` 新增 `RuntimeSettings`，一次解析 permissions/hooks/sandbox/agents/codemode；每个 session 只补 cwd-bound state。
- `main.rs` 统一构建 `UserConfig`、provider settings、runtime settings、MCP/Web source，并删除所有 `PROJECT_CONFIG` 路径与 provider-key 项目检查。
- `mcp_auth.rs` 将 OAuth store 和 login 配置切到用户全局目录。
- plain/TUI 持久化提示、CLI help、README 和 HANDOFF 同步全局作用域。

## 测试

- 全局根 schema 同时接受 provider 与 runtime sections；未知根键与 feature 未知键/坏类型拒绝，secret-safe 错误覆盖。
- malformed 或带不同策略的 cwd `.kloop/config.toml` 不影响 `config/read`；两个 cwd 只在返回的 workspace cwd 上不同。
- `--mock` 得到空、无路径配置快照。
- AllowAlways 私有原子写入、去重、保留其他 section、`0600`、symlink/开放权限/开放目录拒绝；保存后磁盘与进程内 snapshot 同步。
- OAuth token save/refresh、URL 换址失效、`0600`、symlink/开放权限拒绝；sandbox deny-read 同时覆盖 config 与 OAuth store。
- MCP/Web/hooks/sandbox/permissions/agents/codemode 严格 section parser 回归。
- 最终门：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、`cargo run -p kloop -- --mock`。

## 完成记录 ✅（2026-07-24）

- 唯一全局 TOML、进程级 runtime snapshot、全局 AllowAlways/OAuth 持久化与 cwd-only workspace anchor 已落地；legacy cwd config 不再被探测或读取。
- 完成后独立审查补强两条边界：`--mock` 的 provider/runtime 环境覆盖一次性隔离并新增进程级 hermetic 回归；Unix 私有 config/OAuth 读写改为校验并锚定目录 descriptor，再通过 `O_NOFOLLOW` `openat` / `renameat` 操作叶文件；目录创建使用 descriptor-relative `mkdirat` / `chmodat`，临时文件在 rename 前显式 `fchmod(0600)`，严格 umask 下仍保证目录 `0700`、文件 `0600`；叶文件以 `O_NONBLOCK` 打开后再检查 descriptor 类型，FIFO 等非普通文件会立即拒绝而不阻塞。整条路径避免了检查后的 symlink 替换竞态。
- 本机 12 条 permission allow rule 已迁入 `~/.kloop/config.toml`，并补 `[web] search_provider = "tavily"`；provider sections 保持不变，文件保持 `0600`，旧项目 config 已删除。
- 质量门通过：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、`cargo run -p kloop -- --mock`。
- clean-env 真实 provider headless 通过；从含 malformed legacy config 的 cwd 启动仍正常采样。
- clean-env TUI 可从含 malformed legacy config 的 cwd 正常启动；app-server 对两个含不同 legacy config 的 cwd 执行 `initialize` + `config/read`，除 canonical cwd 外返回相同进程级 policy，且未暴露 cwd sentinel。
- 私有配置迁移结构、mode 与 legacy 文件删除状态复核通过；`git diff --check` 通过。
- 提交：本次（见 git log）。

## Plan 63 supersession（2026-08-06）

本页保留 Plan 46 当时的历史事实；其 durable permission 部分已由 Plan 63 替代。当前 `[permissions]` 只接受全局 `deny` / `ask`，legacy `allow` 与非空 `KLOOP_ALLOW` 会在启动时被明确拒绝，不迁移也不作为授权来源。项目批准写入用户私有的 `~/.kloop/projects/v1/<project-id>/permissions.json`，按 ProjectId 共享于同一项目的 session/linked worktree；WorkspaceSession 批准只存在当前 PermissionSession 的 WorkspaceId 分区。`acceptAlways` 已从 native protocol 1.0 删除，由 `acceptForProject` 原位取代；此前协议无人使用，因此不升版本、不保留 alias或双栈。ProjectStore、global config 与 OAuth store 继续共用私有 descriptor/handle-relative I/O 边界，并整体纳入 sandbox private-state deny-read/deny-write。
