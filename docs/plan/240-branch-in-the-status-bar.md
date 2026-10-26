# Plan 240 — 状态栏常显分支与精简键位提示

> 2026-10-26。来源：用户希望页脚一直能看到当前分支；开场会话头会随转录滚进
> scrollback，分支只在那一刻可见。同时 `shift+Tab to change mode · Ctrl+R to rewind`
> 过长，可以精简。
> 状态：✅ 已完成（2026-10-26）。

## 已定取舍

- 分支放在页脚左端、紧跟模式徽标，并排在键位提示之前：窄行先截提示，分支留下。
- 无仓库时整段省略（含分隔符）；超过 24 列的分支以 `…` 截断，worktree 分支名
  再长也不会把提示整个挤掉。
- 分支复用会话已知的那份（启动、worktree enter/exit、`/clear`/rewind 更新），
  不为每帧或每轮额外 spawn `git`；外部终端里 checkout 的分支不会即时反映。
- 提示只去掉赘词：`shift+Tab mode · Ctrl+R rewind · Ctrl+C to exit`，`Ctrl+C` 保留
  `to exit`。启动行、运行中提示、todo 提示的顺序与语义不变。
- 页脚右侧系统状态的让位规则不变：整行放不下时仍只留左侧。

## 实现

- `crates/tui/src/render.rs`：`footer_line` 增加分支段与长度上限；窄行回退路径先保
  徽标与分支，再截提示。修正 `App.branch` 的字段注释（它是会话分支，不只是 worktree）。
- 单测：idle/startup/running 三种页脚都带分支；off-repo 省略；30 列窄行仍见分支；
  长分支截断后提示仍在；精确断言新提示文案。
- PTY 快照中的页脚行同步（夹具目录不是 git 仓库，故无分支段）。

## 验收

- 新增/相关 TUI 单测通过；PTY 快照与实现一致。
- 外层执行沙箱之外完整 `make check` 全绿（fmt、clippy、workspace debug 全量测试、
  本机 parity）。
- `rust/DESIGN.md` 页脚段落改写，`docs/plan/HANDOFF.md` 记实现与教训，一次提交。

## 状态

✅ 已完成（2026-10-26）。提交号以本节所在提交为准。

`footer_line` 左端现在按 `[mode]  分支 · 提示` 组装；分支为空（off-repo）时连同分隔符
一起省略，超过 24 列截断，窄行回退路径先保留徽标与分支。新增单测覆盖 idle/startup/
running、off-repo、30 列窄行与长分支；七份 PTY 快照同步提示文案，effort picker 的
光标列随页脚缩短由 70 变为 57（列表面板不设光标，停在最后写入处）。外层执行沙箱之外
完整 `make check` 全绿，含本机 parity（CLI 156、core 1192、TUI 288、TUI PTY 25、
plain PTY 8）。DESIGN 页脚段落已改写，HANDOFF 已同步并记录教训 270。首版提交 `0d6f17d`。

## 审查修正（2026-10-26）

- **退出 worktree 后分支消失（P2）**：`exit_worktree_tool` 的 `CwdChanged` 用
  `branch: None` 表示“离开 worktree”，页脚把它读成“不在仓库”。core 事件形状被
  worktree parity 报告钉住（`branch: None`），所以在 TUI 的事件边界
  `resolve_cwd_branch` 对 `branch: None` 的 cwd 变化重读 git 分支，再交给 `App`。
  新增临时仓库回归：退出事件补回原分支，已带分支的事件原样保留。
- **补全菜单绕过常显分支（P3）**：`footer_line` 删掉 `popup` 的提前返回，菜单提示改为
  与 idle/startup/running 同一套徽标、分支、截断逻辑；菜单打开时不追加 todo 提示。
  新增回归覆盖完整文案与 20 列窄行仍见分支。
- **新 Git 回归依赖宿主配置（P2）**：临时仓库原先只设局部 user.name/email，仍继承
  全局签名与钩子。现在测试内的 `git` 统一 `env_clear()` 只留 `PATH`，HOME/XDG 指向
  临时目录，`GIT_CONFIG_NOSYSTEM/GLOBAL/SYSTEM` 全部关掉，`LC_ALL=C`，并在仓库本地
  写 `commit.gpgSign=false`、`core.hooksPath` 指向空目录，避免宿主配置让 base 提交
  失败。做法与 plan 59 既有 hermetic git 回归一致。
- 分支修正提交号以本节所在提交为准；完整 `make check` 结果见该提交说明。