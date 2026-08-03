# Plan 56 — Worktree 工具对齐

> 状态：✅ 已完成（2026-07-31）
>
> 母计划：Plan 48
>
> 依赖：Plan 49、Plan 50
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Worktree 会同时改变 cwd、文件/搜索路径、Bash、权限、sandbox、system prompt 和资源清理边界。Plan 56 在 Plan 49 文件工具与 Plan 50 前台 Bash 已稳定后，以精确 2.1.220 fixture 裁决 Enter/Exit，并把 session worktree 与 task isolation 收敛到同一套带所有权的 Git/provenance 底座。

## 最终证据与结论

对应 matrix 行按条件向量拆开：

- `enter-worktree@clean-cli` / `exit-worktree@clean-cli`：只记录注册策略；
- `enter-worktree@worktree-scripted-allow-cli` / `exit-worktree@worktree-scripted-allow-cli`：headless 行为；
- `enter-worktree@worktree-scripted-manual-pty` / `exit-worktree@worktree-scripted-manual-pty`：交互 permission 行为。

最终基线：

- 190 个 capture，raw/normalized 各 190 份；其中 worktree 为 45 组；
- 188 条 static evidence；
- 61 行 × 8 维 = 488 cells：112 compatible / 157 intentional-diff / 34 missing / 151 unknown / 24 n/a / 10 same；
- generated executable pair 仍为 4 个；Plan 56 没有把 native report 或静态相似包装成新的 `same`；
- `kloop-plan56-native-report` 由 `verify.py` 真正执行，并用缺场景、schema 篡改、cwd 事件重排、external 删除、主树泄漏和 stale permission gate 做 negative mutation；
- clean profile 的注册差异固定为 `intentional-diff`：kloop 只在 depth 0 且 `SurfaceCapabilities.worktree=true` 时注册。

## 已落地输入契约

### Enter

`enter_worktree` 接受 strict object：

```json
{"name": "feature/name"}
{"path": "/registered/worktree"}
{}
```

- `name?: string` 与 `path?: string` 互斥；均省略时生成名称；
- 显式 null、错类型、未知字段和 name/path 同传都拒绝；
- name 最多 64 字符，支持 `/` 分段；每段只允许 ASCII 字母、数字、点、下划线和短横线，并拒绝空段、`.`、`..`、绝对路径与逃逸；
- `/` 在受管目录/分支标识中编码为 `+`；受管目录为 `.claude/worktrees/<encoded>`，分支为 `worktree-<encoded>`；
- `path` canonicalize 后必须存在、与当前 repository 的 Git common-dir 相同且已出现在 `git worktree list --porcelain`；通过 path 进入只取得 External custody，不取得删除权。

### Exit

`exit_worktree` 接受 strict object：

```json
{"action": "keep"}
{"action": "remove", "discard_changes": true}
```

- `action` 必填，只能是 `keep | remove`；`discard_changes` 可选且必须为 boolean；
- `keep` 总是恢复原 cwd 并保留 worktree；
- `remove` 只允许删除当前 session 创建且 provenance 仍一致的 Managed worktree；
- tracked/staged/unstaged/untracked/ignored 变化或 base 后 commit 均阻止默认 remove；只有成功观测到的内容/commit 变化可由 `discard_changes:true` 覆盖；
- path/repository/common-dir/registration/branch/base/owner 验证失败不能由 discard 覆盖；拒绝或失败时保留 active handle 与 worktree cwd，供用户 keep 或修复后重试；
- External、previous-session、task-owned tree 永远不能由 session Exit 删除。

## 共享底座与所有权

`core/src/worktree.rs` 现在统一承载：

- canonical repository/common-dir、canonical path、registered entry、expected branch、base commit；
- `Managed | External` custody；
- `Session | Task` owner；
- active/removing/kept/removed/orphaned lifecycle；
- 同一 Git common-dir 上的 process-local async mutation lock。

新建 session tree 从 fresh remote base（可用时）创建；task isolation 继续从当前 HEAD 创建。两者共用创建、provenance、change inspection 和 cleanup 机件，但 active slot、cwd、删除权和终态完全隔离。普通 shared child 继承父 effective cwd；isolated task 获得独立 task-owned handle。task clean 时自动清理，dirty/commit 或 probe failure 时保留并回灌诊断；session shutdown 没有显式 remove intent，因此连 clean active tree 也保守 retain。

`Permissions::rebased` 共享 session-global mode 与动态 AllowAlways 规则，只重置 per-workspace session approval cache；在 worktree 内批准的 AllowAlways 退出后仍立即对 base gate 生效。

## Effective context 与前端

Enter 成功后 `Config.effective_*` 同步切换：

- cwd；
- permissions；
- sandbox writable root；
- fresh FileState；
- system prompt 的 `Working directory:`。

真实 dispatcher golden 覆盖 Read/Write/Edit/Glob/Grep/Bash，确认相对路径只落 worktree、主树无泄漏。只有真实 cwd transition 成功后才发 `Event::CwdChanged`：进入时带 worktree branch，成功 keep/remove 后 branch 为 null；无 active Exit 和 remove refusal 不发伪 transition。TUI 同步 cwd/branch/header/search root；server 继续通过现有 cwd notification 投影变化，工具是否可见仍由 surface capability 决定。

## 有意差异

- kloop 保留 capability-gated registration，不复制 clean CLI 的无条件可见性；
- kloop 的 no-active Exit 返回明确 tool error；固定目标输出 no-op；
- kloop 把 ignored content、provenance 不可验证和 external ownership 当成 removal blocker；
- session shutdown 保留 active tree，而不在缺少显式 remove intent 时删除；
- 不自动 merge、rebase、commit、push 或丢弃用户改动；
- 不把 native report 或结构相似升级成无 executable comparator 的 `same`。

## Fixture 与测试

Exact fixture 覆盖：

- strict schema/parser、默认 name、nested name、path missing/unregistered、重复 Enter；
- create/remove clean、existing keep/switch、keep dirty、tracked/staged/untracked/ignored、dirty/commit discard；
- interactive approve/reject/cancel 与 existing remove；
- no-active Exit、same-round Enter/Exit、concurrent Enter/Exit、shutdown active state；
- 每例独立临时 Git repo，保存 registry/branch/base/HEAD/status/cleanup 前后状态并审计无残留。

Rust 覆盖：

- schema/parser、managed/external ownership、provenance mismatch、existing path；
- ignored/dirty/commit fail-closed 与显式 discard；
- cwd/system/file/search/Bash 重锚；
- session/task 不互删、并发 mutation 串行、shutdown retain；
- TUI/server cwd 投影；
- rebased AllowAlways 动态规则共享；
- executable native report 与 Python tamper-negative gate。

## 验证

```bash
python3 -B refs/claude-code-2.1.220/build_matrix.py --check
python3 -B refs/claude-code-2.1.220/verify.py
python3 -B refs/claude-code-2.1.220/verify.py --corpus-only
cd kloop
cargo test -p kloop-core worktree
cargo test -p kloop-core plan56
cargo test -p kloop-server worktree
cargo test -p kloop-tui worktree
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --plain --mock
cd ..
git diff --check
```

完成记录：实现、exact corpus、native report、matrix、文档和全量门禁在一次 `feat(plan56)` 提交中闭合。
