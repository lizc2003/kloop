# Plan 63 — Project 作用域、Config 生命周期与权限归属

> 状态：未开工
>
> 依赖：Plan 8、35、37、39、46、49、51
>
> 施工关系：与 Plan 61、62 的 `Config`、permission、sandbox、startup 修改面高度重叠，不得并行；推荐先完成本计划，再按新 seam 实施 Plan 61/62。
>
> 规划基线：kloop `a42f1b8`

## 背景

当前主链分层（protocol → provider → core → frontend/CLI）合理，但 `core::Config` 内部没有表达真实作用域：process/global、project、session、workspace 和 agent 五种生命周期被装进同一个可 `Clone` 的扁平对象。

这个问题已经越过“字段较多”的代码卫生层面：

- provider/model、全局 hooks/MCP sources、cwd/project prompt、permission、session registries、agent-local todo/inbox/file state 和 active worktree 都在 `Config`。
- sub-agent 通过 `Config { ..cfg.clone() }` 后手工替换若干 `Arc` 来决定共享与隔离；新增字段时，编译器无法证明它属于哪一层。
- `Permissions` 同时拥有规则、mode/pre-plan、session cache、cwd、approver 和持久化 sink。
- `AllowAlways` 把项目中的一次批准写进全局 `~/.kloop/config.toml`，之后影响所有 workspace；`cwd` 只提供路径锚点，不能表达项目归属。
- `Permissions::rebased()` 复制动态 allow，导致 base/worktree gate 的 live policy 可以分叉。
- worktree sandbox 通过追加 writable root 继承主 checkout root；sandbox auto-allow 下，隔离 worktree 仍可能以绝对路径写主树。

Plan 46 删除仓库内项目 TOML 是正确的信任边界：仓库内容不能给自己授权。本计划不恢复 repo-controlled config，而是在用户私有状态中建立 Project identity 与 project-scoped durable permission，并以显式构造 API 固定 Config 的生命周期所有权。

## 已确认的产品决定

1. 不保留旧 global allow 或旧 approval wire 的兼容行为；按新的作用域模型一次纠正。
2. `~/.kloop/config.toml [permissions]` 只保留全局 `deny` / `ask` 约束；删除 `allow`。
3. 删除 `KLOOP_ALLOW`；保留 process-global `KLOOP_DENY` / `KLOOP_ASK`。
4. legacy `[permissions].allow` 与非空 `KLOOP_ALLOW` 不自动迁移、不静默忽略、不改写用户配置；启动时以不回显规则内容的 actionable error 拒绝，用户删除后在各项目重新批准。
5. 所有交互式 durable grant 都归当前 ProjectId；不再提供 global/always 批准。
6. 仓库内 TOML、AGENTS/CLAUDE、`.kloop/rules`、Git config、branch、remote URL 均不能成为授权来源。
7. server approval contract 直接升级为 protocol 2.0；删除 `acceptAlways`，不做 v1/v2 双栈。
8. session/offload/program-run 不迁入 ProjectStore；本计划只迁 permission ownership。
9. worktree sandbox root corrective 同计划完成，因为它是 Project/Workspace 作用域混淆造成的直接安全缺陷。

## 必须保持的不变量

- permission gate 顺序继续是：global deny → sensitive-read hard block → plan read-only gate → safety checks → global ask → sandbox auto-allow → bypass → read-only verdict → accept-edits → project allow → workspace-session cache → human approval。
- deny 永远高于 allow；敏感读取不可审批、不可缓存、不可 bypass；危险命令与敏感写入继续 bypass-immune。
- explicit ask 高于 sandbox auto-allow、project allow 和 session cache。
- opaque Bash 在没有 containment 时不能命中 allow、不能记 session/project grant。
- file mutation 继续在 approval 前准备并冻结 parent/target capability；permission 与 executor 必须消费同一 resolved target，不能在等待后重新按 pathname 解析。
- `--mock` 的 allow-all 仍是 hermetic 测试特例，不等于 CLI bypass。
- 每个 server thread 的 mode、cache、approver 和后台 registry 独立；同项目 thread 只能共享 project policy。
- sub-agent 共享父 session 的 mode/approver/background registries，但 todo、inbox、file observations 和 active-worktree cursor 必须 fresh。
- worktree 与主 checkout 可共享 durable project policy，但不能共享 WorkspaceId 级 AllowSession cache。

## 1. Project、Workspace 与身份解析

新增 `kloop/crates/core/src/project.rs`，定义不可混用的强类型：

```rust
pub struct ProjectId(/* opaque digest */);
pub struct WorkspaceId(/* opaque digest */);

pub struct WorkspaceIdentity {
    pub project_id: ProjectId,
    pub workspace_id: WorkspaceId,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    project_anchor: PathBuf,
}
```

### Git workspace

对 canonical cwd 做一次受控 Git probe：

```text
git -C <cwd> rev-parse --path-format=absolute \
  --is-inside-work-tree --show-toplevel --git-common-dir
```

固定语义：

- `project_anchor` = canonical `--git-common-dir`。
- `workspace_root` = canonical `--show-toplevel`。
- ProjectId = `SHA-256("kloop-project/git/v1\0" || canonical-common-dir-bytes)`。
- WorkspaceId = `SHA-256("kloop-workspace/git/v1\0" || canonical-top-level-bytes)`。
- linked worktree 共享 common dir，因此 ProjectId 相同；checkout root 不同，因此 WorkspaceId 不同。
- submodule common dir 独立，视作独立 project。
- 不使用 branch、HEAD、remote URL、repo 内容或 `--git-dir` 生成身份。

### 非 Git workspace

- 只有 Git 明确报告“not a repository”时才进入非 Git 分支。
- `project_anchor = workspace_root = canonical cwd`。
- ProjectId/WorkspaceId 使用不同 domain separator 对 canonical cwd 求完整 SHA-256。
- 不向上猜测 marker，不按目录名、内容或 remote-like 文件归并项目。

### 降级与安全

- ID 只保证本机当前 common-dir/path 的稳定分区，不承诺跨 clone、跨机器、移动或删除重建后稳定。
- digest 只作为私有 store key；raw anchor 不进入 model-visible output、server approval request 或 `config/read`。
- Git executable/probe/canonicalization 异常时，会话仍可使用 once/session approval，但 project policy 标记 unavailable，不读取或写入 durable grant。
- 新建 linked worktree 后必须重新解析 identity，并断言 ProjectId 与父项目一致；异常或不一致时 worktree 操作 fail closed，clean 新树按既有安全清理规则撤销。

## 2. Config 的目标所有权

保留 `Config` 作为 agent loop 和 `ToolCtx` 的窄 composition façade，但取消公开 blanket `Clone` 与 struct-update 派生。目标由四层对象组成，Workspace 作为 agent 的执行视图单独建模。

### `RuntimeServices` — process scope

`Arc` 共享：

- provider transport；
- process 默认 model/context/fallback；
- hooks；
- MCP/Web `ToolSource` connections；
- agent type definitions；
- code-mode limits与 defer threshold；
- immutable `GlobalPermissionPolicy`（deny/ask）；
- `ProjectPolicyRegistry`。

MCP connection 和其他 process resources 继续由 composition root 拥有，不进入 ProjectStore 或 session shutdown。

### `ProjectContext` — project scope

- `ProjectId`；
- shared `ProjectPermissionPolicy`；
- project policy availability/status。

同一 Git common dir 的 worktree、sub-agent 和 server thread可取得同一 policy handle；不同 ProjectId 必须隔离。

### `SessionRuntime` — session/thread scope

- session id；
- `PermissionSession`：mode、pre-plan、approver、按 WorkspaceId 分区的 cache；
- background task/shell registries；
- unlocked tools；
- existing session/offload storage paths；
- session shutdown ownership。

每个 CLI session或 server thread 新建一份；sub-agent共享。rollout 不持久化 mode、cache、approver、policy snapshot或 revision。

### `AgentContext` 与 `WorkspaceState` — agent/workspace scope

`AgentContext` 拥有：

- model/system/max-rounds/label/tool allowlist；
- todos、inbox、file state；
- base `WorkspaceContext` 与 active-worktree cursor。

`WorkspaceContext` 包含：

- `WorkspaceIdentity`、effective cwd；
- assembled project instructions/system snapshot；
- skills snapshot；
- workspace-instantiated sandbox；
- `PermissionWorkspace` view。

`WorkspaceState` 一次读取返回完整 `EffectiveWorkspace` snapshot。一个 tool call 从 hook 后的 prepare、permission、sandbox verdict 到 executor 均使用同一 snapshot，不再分别读取 `effective_cwd()`、`effective_permissions()`、`effective_sandbox()`、`effective_system()` 造成混合时代视图。

### 显式构造

新增窄构造器/测试 builder：

- main session；
- shared-workspace sub-agent；
- agent-type override；
- isolated-worktree sub-agent；
- active-worktree transition。

删除 `Config { ..(*ctx.cfg).clone() }`。构造器必须显式编码“共享 session、共享 project、fresh agent-local state、按 isolation 选择 WorkspaceId”的规则；以后新增字段必须在构造 API 中决定所有权。

## 3. Permission 分层

`Permissions` 可保留为调用 façade，但内部拆成：

```text
GlobalPermissionPolicy   immutable global deny/ask
ProjectPermissionPolicy  shared durable allow snapshot + writer
PermissionSession        mode/pre-plan/approver/workspace caches
PermissionWorkspace      project + session + WorkspaceIdentity
PermissionCall           tool/input/depth/resolved target/sandbox fact
```

### Policy 规则

- global config 不再提供 allow。
- ProjectStore v1 只存 durable `allow`；project grant 不能撤销 global deny/ask 或内建 hard block。
- gate 不做“后层覆盖前层”的扁平 merge，而是在各自层按优先级查询。
- project allow成功持久化后，对 registry 中所有同 ProjectId policy view原子可见，包括已经创建的 sibling thread 和 worktree。
- AllowSession cache 以 `(PermissionSession, WorkspaceId)` 分区：同一 workspace 的 sub-agent沿用，isolated worktree从空 cache开始，退出 worktree后 base cache仍在。
- mode/pre-plan 属于 PermissionSession；同一 session的 base/worktree/sub-agent双向可见，不同 server thread不共享。
- 删除 `Permissions::rebased()` 的规则复制；workspace切换只产生新的 `PermissionWorkspace`。

### Per-call facts

- cwd、WorkspaceId、prepared target 和 sandbox containment均是 call fact，不进入 durable policy。
- file path规则继续同时看到 original spelling与 executor冻结后的 resolved target。
- sandbox auto-allow只能来自当前调用实际会受 containment 的事实；`disable_sandbox=true` 永远不能享受它。
- sandbox escalation继续只批准撤销 containment的重跑，不生成 session/project allow。

## 4. 用户私有 ProjectStore

### 布局

```text
~/.kloop/projects/v1/<project-id>/permissions.json
~/.kloop/projects/v1/<project-id>/permissions.lock
```

仓库内不创建 permission config；ProjectStore 是 application-owned state，不是第二套自动 TOML。

### JSON v1

```json
{
  "version": 1,
  "projectId": "p1_<full-sha256>",
  "revision": 3,
  "allow": ["bash(cargo test *)"]
}
```

要求：

- `deny_unknown_fields`；
- version 必须等于 1；
- projectId 必须与目录和当前 identity一致；
- revision 单调递增，溢出拒绝；
- publish 前全部规则走 core唯一 parser；
- exact dedup，稳定写出顺序；
- missing file = revision 0空 policy。

v1 只保存 allow，使 malformed/unavailable store可以安全降级为“没有 durable grant”；project-specific deny/ask若未来出现，需另立 schema与管理入口，不能在 allow-only store里预埋未定义语义。

### Private I/O

从 `cli/src/user_config.rs` 抽出 `cli/src/private_store.rs`，供 global config、OAuth 和 ProjectStore复用：

- 从用户私有 `~/.kloop` directory descriptor安全 walk/create `projects/v1/<id>`；
- 中间目录 0700，拒绝 symlink和非目录；
- leaf/lock必须 regular file、0600，拒绝 symlink/FIFO/device/socket；
- same-directory exclusive temp、显式权限、file sync、atomic rename、directory sync；
- 错误不回显配置或规则原文；
- non-Unix保持现有安全边界，不借本计划实现 Plan 61/62 的 Windows文件 capability改造。

### 并发与发布

单靠 atomic rename无法避免两个进程同时 append丢更新。固定流程：

1. 安全打开固定 `permissions.lock`；
2. 获取 OS advisory exclusive lock；
3. 锁内重新读取并严格校验最新 JSON；
4. dedup/append requested allow，revision + 1；
5. private atomic replace；
6. 解锁并返回完整 snapshot；
7. 同进程 per-project async lock再保证 snapshot swap顺序。

blocking lock/RMW走 `spawn_blocking`。不同 ProjectId 使用不同 lock并可并行。进程崩溃通过 descriptor lock自动释放，不使用 stale sentinel lockfile协议。

### 失败语义

- durable-first：磁盘成功后才发布内存 project snapshot。
- 用户选择 project但 lock/read/write/fsync/rename失败时，本次明确批准按 once执行；内存 policy不更新，并显示“未持久化”通知。
- 已存在但 malformed/insecure/unreadable/ID mismatch的 store不加载任何 project allow，project scope标记 unavailable并给显著、secret-safe warning；once/session仍可用。
- headless不能修复或创建 policy；需要项目规则才能放行的调用按现有无 approver语义拒绝。
- ProjectStore不拥有 rollout/offload、MCP、background worker、thread或 worktree lease。

## 5. Approval UI 与 native protocol 2.0

### Core decision

```rust
pub enum ApprovalScope {
    Once,
    WorkspaceSession,
    Project,
}

pub enum Decision {
    Allow(ApprovalScope),
    Deny,
}
```

`ConfirmRequest` 明确给出允许的 scopes。只有 rememberable call才广告 WorkspaceSession/Project；opaque Bash、explicit ask、sensitive path、sandbox escalation和 plan exit只允许 once/deny。

### Plain/TUI

- `y`：allow once；
- `a`：allow for this session in this workspace；
- `p`：allow for this project across sessions and linked worktrees；
- `n`：deny。

前端不能仅靠 `remember_rules.is_some()` 猜 scope；按 request提供的 scopes渲染。成功提示明确 project作用域，不再出现“all workspaces”。

### Server breaking wire

- `PROTOCOL_VERSION` 从 `1.0` 升为 `2.0`；握手继续 exact-match。
- initialize capability把 approval从 bool改为包含 scopes的结构化 capability。
- `approval/request` 保留 `rememberRules`，新增 `approvalScopes`。
- response只接受：
  - `accept` → Once；
  - `acceptForSession` → WorkspaceSession；
  - `acceptForProject` → Project；
  - `decline` → Deny。
- 删除 `acceptAlways`；旧值、unknown、missing、cancel、EOF全部 deny。
- 不实现 protocol v1兼容或 token alias。

同步更新 `~/work/桌面前端仓库/app` 的 kloop专用分支：握手 v2、approval scopes、`acceptForProject`和对应前端文案。kloop main最终一次 Plan 63 commit；app仓库按其分支纪律做独立 companion commit。

## 6. Server、resume、sub-agent 与 worktree

### Server

保留现有 `ConfigFactory` seam：

1. canonicalize `ThreadStartOptions.cwd`；
2. resolve WorkspaceIdentity；
3. 从 registry取得 ProjectPermissionPolicy；
4. 每 thread新建 PermissionSession和后台 registries；
5. 装配 workspace prompt/skills/sandbox；
6. 通过 main-agent constructor创建 Config。

同 ProjectId thread共享 live project allow；mode/cache/approver/worker绝不共享。`config/read`继续是安全 allowlist DTO，不返回 ProjectId、policy path、rules或 revision。

### Resume/fork

- rollout继续只持久化 cwd/model等既有 runtime metadata，不写 permission state。
- resume用 stored cwd重新 canonicalize和解析当前 ProjectId/WorkspaceId，再加载当前 project policy。
- resumed thread获得 fresh mode/cache/approver；policy在停机期间变化时自然读取新 snapshot。
- cwd缺失或无法建立安全 workspace时拒绝 resume，不猜 project。

### Sub-agent

显式 constructor固定：

- 共享 RuntimeServices、ProjectContext与父 SessionRuntime；
- shared workspace沿用同 WorkspaceId和 cache partition；
- fresh todos/inbox/file state/active-worktree cursor；
- agent type只覆盖 model/system/tool allowlist；
- isolated worktree创建新 WorkspaceContext/WorkspaceId，ProjectId必须相同，cache为空。

## 7. Worktree sandbox corrective

当前 `with_writable_root()` 会保留主 checkout root。改为：

- `SandboxPolicy` 区分 workspace-derived root、tmp roots和用户显式 `[sandbox].writable_roots`。
- 进入/创建 worktree时以新 workspace root替换旧 workspace-derived root。
- tmp/TMPDIR和用户显式 extra roots保留。
- 主 checkout只有被用户显式配置为 extra root时才继续可写。
- task isolation与 session active worktree使用同一 workspace sandbox constructor。
- denied-read继续覆盖全局 config、OAuth和当前 ProjectStore路径。
- sandbox auto-allow位置不变：worktree内绝对写主树应在 containment中失败，只有明确批准 escalation才可无 sandbox重跑。

无 sandbox平台上的 worktree仍只保证普通相对路径和并行编辑隔离，不宣称抵御模型主动使用绝对路径写主树；README必须明确该边界。

## 8. 非目标

- 不迁移 `.kloop/sessions`、offload、program-runs或 background output；不建立跨项目 session catalog。
- 不改变 rollout JSONL格式、subagent lineage或 resume picker。
- 不在 enter-worktree后重新发现 instructions/skills/commands；保持当前 session snapshot语义。
- 不引入 repo trust、项目 TOML、permission editor或 global allow。
- 不把 ProjectStore变成所有持久化状态的杂物箱。
- 不重构 MCP connection、background registries或 Team/Task control plane。
- 不顺带实施 Plan 61文件纠偏或 Plan 62 Windows shell；三者必须串行。

## 9. 实施切片

所有切片保留在同一 working tree，以 focused tests作 checkpoint；kloop最终只做一次 `plan63` commit。

### 切片 0：基线与安全契约

- 复核 Plan 61/62尚未实施且工作树无冲突修改。
- 固定 permission gate顺序、prepared target、AllowAlways/global persistence、sub-agent clone、server approval和 sandbox root现状测试。
- 新测试先表达目标 breaking contract，不用中间兼容 adapter过渡。

### 切片 1：Identity、private store 与 ProjectStore

- 实现 ProjectId/WorkspaceId解析和 Git/non-Git fixtures。
- 抽 private store primitive并让现有 global config/OAuth复用。
- 实现 allow-only schema、advisory lock、RMW、revision和 registry。
- 此阶段先不接 permission runtime；pure/focused tests全绿。

### 切片 2：Permission policy/session/workspace拆分

- 引入 GlobalPermissionPolicy、ProjectPermissionPolicy、PermissionSession、PermissionWorkspace和 PermissionCall。
- 迁移现有 gate tests并保持顺序。
- 接 project allow、WorkspaceId cache、durable-first发布和失败降为 once。
- 删除 global allow/KLOOP_ALLOW与 `Permissions::rebased()` dynamic policy copy。

### 切片 3：Config 生命周期与 call snapshot

- 引入 RuntimeServices、ProjectContext、SessionRuntime、AgentContext、WorkspaceState/Context。
- 删除 Config blanket Clone和 struct-update派生。
- 改 main/sub-agent/agent-type/worktree构造路径与统一 test builder。
- dispatch固定一次 EffectiveWorkspace；prepared target、permission、sandbox和 executor共用它。

### 切片 4：CLI/server/frontends 与 sandbox

- 重写 `RuntimeSettings`/`config_from_settings` composition；global只解析 deny/ask。
- 接 plain/TUI scoped decision和 ProjectStore通知。
- server升级 protocol 2.0，接 per-thread identity/policy/session和 resume重解析。
- sandbox workspace root从 append改成 replace；补 task/session worktree回归。

### 切片 5：Desktop companion、文档与验收

- 更新 app kloop分支 protocol/approval adapter与测试。
- 更新 README、HANDOFF、capability report和 Plan 46 supersession note。
- 运行全部 focused与 workspace门，回填本计划完成记录。
- kloop main一次提交，信息包含 `plan63`；app专用分支独立 companion commit，不推送除非用户另行要求。

## 10. 测试矩阵

### Identity

- main checkout与两个 linked worktree：ProjectId相同、WorkspaceId各异。
- nested cwd：WorkspaceId保持 checkout identity，effective cwd保留调用者目录。
- 独立 clone/init：ProjectId不同。
- non-Git canonical cwd与 symlink alias稳定。
- branch/HEAD/remote变化不改变 identity。
- Git probe异常、bare repo、canonicalization失败：project scope unavailable，不误套其他 policy。
- storage key只含固定ASCII digest，不含 raw path。

### ProjectStore

- missing store → empty policy。
- version/unknown field/projectId/revision/invalid rule严格拒绝加载。
- 0700/0600、umask、symlink中间目录/leaf、FIFO/non-regular、atomic temp cleanup。
- 同进程与多进程并发 grant最终为规则并集，无 lost update。
- writer崩溃后锁可回收，无 stale sentinel。
- duplicate grant幂等；revision单调。
- persist失败不更新内存、不显示成功，本次调用仅 once。
- project grant不修改 `~/.kloop/config.toml`。
- 两个 ProjectId的文件、lock和 live snapshot完全隔离。

### Breaking migration

- global `[permissions].allow` 给 secret-safe actionable startup error。
- 非空 `KLOOP_ALLOW`同样拒绝。
- global deny/ask及 KLOOP_DENY/ASK继续生效。
- 仓库 `.kloop/config.toml`仍完全不读取，不能授予能力。

### Gate 与作用域

- global deny + project allow → deny。
- global ask + project allow/sandbox → ask。
- project allow不能越过 sensitive、plan、destructive safety。
- opaque Bash、explicit ask、sandbox escalation、plan exit不生成 project/cache grant。
- same WorkspaceId sub-agent共享 AllowSession；isolated worktree不继承。
- mode/pre-plan在同 session base/worktree/sub-agent间共享；server thread间隔离。
- project grant对已创建的同项目 base/worktree/sibling thread立即可见，对另一 ProjectId不可见。

### Config/agent ownership

- sub-agent fresh todo/inbox/file state/active cursor。
- shared Runtime/Project/Session ownership与 agent type override不串改 parent。
- server thread共享 project policy时不共享 mode/cache/registries。
- 新增生命周期字段时统一 constructor/test builder强制处理，不再有生产 struct-update clone。

### TOCTOU 与 sandbox

- prepared read/mutation、original alias、resolved target、parent swap、approval wait回归不退化。
- 一个 tool call在 active-worktree切换竞态中只观察一个 EffectiveWorkspace。
- worktree sandbox含新 root、不含隐式主 root；tmp和 explicit roots保留。
- sandboxed worktree绝对写主 checkout失败且未落盘。
- `disable_sandbox=true`不享受 auto-allow，仍走原 gate/escalation。
- ProjectStore路径继续被 sensitive/denied-read保护。

### UI、wire、headless

- plain/TUI只对 rememberable request显示 y/a/p/n并使用准确scope文案。
- server只接受 protocol 2.0与 `acceptForProject`；v1 handshake失败，`acceptAlways`按 unknown deny。
- approval request广告 once/workspaceSession/project scopes，不泄漏 ProjectId/path/policy内容。
- disconnect/cancel/unknown/missing response仍 deny。
- headless不能写 ProjectStore；existing project allow可命中，其余需要审批的调用deny。
- `--mock`不读 HOME、不运行 Git、不创建 projects目录。

### Server/resume/Desktop

- 两项目 thread策略隔离；同项目 thread live policy共享、session state隔离。
- resume按 stored cwd重新解析并读取当前 policy，不序列化 rule/mode/cache。
- invalid restored cwd拒绝。
- Desktop kloop adapter完成 v2握手、project approval与错误路径E2E；旧v1明确不连接。

## 11. 关键文件

新增：

- `kloop/crates/core/src/project.rs`
- `kloop/crates/cli/src/private_store.rs`
- `kloop/crates/cli/src/project_store.rs`

核心修改：

- `kloop/crates/core/src/config.rs`
- `kloop/crates/core/src/permissions.rs`
- `kloop/crates/core/src/tools/mod.rs`
- `kloop/crates/core/src/tools/task.rs`
- `kloop/crates/core/src/worktree.rs`
- `kloop/crates/core/src/sandbox/mod.rs`
- `kloop/crates/cli/src/startup.rs`
- `kloop/crates/cli/src/user_config.rs`
- `kloop/crates/cli/src/main.rs`
- `kloop/crates/cli/src/ui.rs`
- `kloop/crates/tui/src/{app.rs,render.rs,lib.rs}`
- `kloop/crates/server/src/{lib.rs,wire.rs}` 与 server tests
- 受 Config literal/clone影响的统一 test builders与模块测试
- `~/work/桌面前端仓库/app` 的 kloop专用分支 adapter（实施阶段）

文档：

- `kloop/README.md`
- `docs/plan/HANDOFF.md`
- `docs/capability-report.md`
- `docs/plan/46-global-user-config.md` 只补 supersession note，不改写历史记录
- Plan 61/62在实施前按新 seam复核，不提前机械改写

## 12. 验证

实施阶段至少运行：

```bash
cd kloop
cargo test -p kloop-core project::tests
cargo test -p kloop-core permissions::tests
cargo test -p kloop-core worktree::tests
cargo test -p kloop-core sandbox::tests
cargo test -p kloop-core tools::task::tests
cargo test -p kloop project_store::tests
cargo test -p kloop private_store::tests
cargo test -p kloop startup::tests
cargo test -p kloop-server approval
cargo test -p kloop-server resume
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

另外执行：

- temporary Git repositories + linked worktrees identity测试；
- 多进程 ProjectStore lock/RMW测试；
- server protocol 2.0 duplex E2E；
- app kloop分支 handshake/project approval E2E；
- 人工检查真实用户 config不会被实现代码静默迁移或改写。

## 完成标准

- Project/Workspace/Session/Agent作用域由类型和显式 constructor表达，不再依赖 Config clone约定。
- durable allow只按 ProjectId存于用户私有 state；global allow与旧 `acceptAlways`完全移除。
- 同项目 live policy共享、WorkspaceId cache隔离、server thread session状态隔离均有确定性测试。
- permission gate顺序、prepared-target安全和 sandbox/approval coupling无回退。
- worktree sandbox不再隐式保留主 checkout writable root。
- server protocol 2.0与 Desktop adapter同步完成，不保留双栈。
- 所有门禁全绿，kloop一次提交，提交信息含 `plan63`；不推送远端。
