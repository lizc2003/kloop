# Plan 56 — Worktree 工具对齐

> 状态：未开工
>
> 母计划：Plan 48
>
> 依赖：Plan 49、Plan 50
>
> 固定目标：Claude Code 2.1.220；版本身份以 Plan 48 manifest 为准。

## 背景

Worktree 会改变 cwd、文件路径、Bash 执行和清理边界，因此必须等待 Plan 49 文件工具与 Plan 50 前台 Bash 稳定。

Plan 48 只在 `worktree=false` 的 clean profile 观察了 EnterWorktree/ExitWorktree。当前 matrix 将 registration 标为 `intentional-diff`、schema 标为 `compatible`，parser 到 lifecycle 全部未知；但现有 schema evidence 没有覆盖 kloop 的真实工具定义，不能据此判 schema parity。

## 当前证据与差距

对应 matrix 行：

- `enter-worktree@clean-cli`
- `exit-worktree@clean-cli`

当前结论：

- CC clean fixture 中 Enter/ExitWorktree 可见；kloop 注册受 `worktree_enabled` 条件控制。
- CC Enter schema 支持可选且互斥的 name/path，Exit schema 包含 action/discard；kloop 当前工具定义的字段与默认行为不同，现有 `compatible` 必须重评。
- 现有 matrix 引用了不覆盖 registration/schema 的 worktree code locator；已有 tests locator 也未挂到适用维度。
- create/enter、keep/remove、dirty/clean、uncommitted/unmerged commits、取消和异常 cleanup 均未做成对 fixture。
- task isolation 与 session worktree 的关系未被 Plan 48 裁决。

优先复用：

- `kloop/crates/core/src/worktree.rs`
- kloop worktree tool 注册/dispatch
- Plan 49 的路径/cwd 契约
- Plan 50 的 Bash cwd、permission 和进程清理契约
- 现有 worktree clean/dirty/cleanup tests

## 目标

1. 固定 EnterWorktree、ExitWorktree 的注册条件、schema、parser 和错误。
2. 固定创建新 worktree、进入已有 worktree、名称/path 校验和 cwd 更新。
3. 固定 keep/remove、dirty/clean、未提交文件、未合并 commit 和取消语义。
4. 固定 session exit、异常、并发 create/remove 和 process cleanup。
5. 固定 task isolation 与会话 worktree 的边界。
6. 全部 fixture 使用独立临时 Git repo，不触碰当前仓库工作树。

## 开工证据闸门

- 建立 `worktree=true` 的真实 CC profile，记录 feature、入口和完整条件向量。
- 从 exact bundle 追 Enter/Exit 的 gate、schema、Git 操作、permission、result 和 cleanup。
- 将现有 `static:kloop-worktree-tests` 引用到适用维度，并为 `tools/worktree_tool.rs` 的 registration/schema/parser 增加准确 locator。
- collector 每个 case 新建临时 repo、初始 commit 和受控 dirty/commit 状态。
- raw capture 保存 Git 前后状态；临时路径/branch ID 只按声明规则归一化。
- 任何 remove case 都只能删除本次 fixture 创建的 worktree；目标与描述不符时 fail closed。

## 实施切片

### 0. 注册与输入契约

- name/path 互斥、默认 name、已有路径、非 Git repo、nested worktree。
- CC 可见性与 kloop `worktree_enabled` gate 的差异。

### 1. Enter 生命周期

- 创建/进入、base ref、branch、cwd 更新、重复 enter 和失败恢复。
- 验证 Read/Write/Edit/Glob/Grep 与 Bash 都使用新 cwd，不泄漏主树。

### 2. Exit 生命周期

- keep/remove、clean/dirty、uncommitted files、unmerged commits、discard flag。
- 取消、断线、异常和清理后的 cwd 恢复。

### 3. 并发与 task isolation

- 同名 create、并发 remove、一个任务一个 worktree 和父子 cwd。
- 不为内部统一牺牲已证明的隔离属性。

### 4. 产品与回归

只实现已裁决差距；注册策略不同可保留 `intentional-diff`。同步 matrix、fixture、evidence 和 verifier。

## 非目标与有意保留

- 不自动 merge、rebase、commit、push 或丢弃用户改动。
- 不操作 kloop 当前工作树或用户仓库。
- 不重写 Plan 49/50 已固定的路径和 Bash 语义。
- 不把 task isolation 与 session worktree 默认合并。
- 不推断其他 VCS、远程 worktree 或未运行平台。

## Fixture 与测试

至少覆盖：

- name/path/default、非法名、路径不存在、非 Git、已有 worktree；
- create/enter 成功、重复进入、失败回滚；
- exit keep/remove、clean/dirty、uncommitted/unmerged、discard；
- 并发同名、remove 冲突、取消和 session exit；
- 文件工具与 Bash cwd 一致；
- fixture 后无遗留 worktree、branch 或进程。

验证：

```bash
python3 -B refs/claude-code-2.1.220/verify.py
cd kloop
cargo test -p kloop-core worktree
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kloop -- --mock
cd ..
git diff --check
```

## 文档同步

完成时同步本 plan、HANDOFF、refs/README、kloop README、capability report 与 parity 产物。明确注册 gate、dirty policy 和 task isolation 的实际边界。

## 完成标准

- Enter/Exit 当前可运行链有 exact fixture 与 kloop golden。
- 临时 Git repo 外无文件/branch/worktree 改动。
- clean/dirty/commit/取消/异常 cleanup 均有确定性测试。
- 所有门禁全绿，一次提交，提交信息带 `plan56`。

## 开工时定 / 问用户

- kloop 是否继续条件化注册，还是提供 CC-visible compatibility surface。
- dirty worktree 的默认保留、拒绝和 discard 交互。
- task isolation 与 session worktree 是否共享底层资源模型。
