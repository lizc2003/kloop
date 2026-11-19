# Plan 288 — `ps` 在沙箱里能跑；打印进程环境变量的 `ps` 先问 ✅

> 2026-11-19 立并完成。来源：plan 283 第十三节后续候选第 6 条。用户同意按“profile 只给 `/bin/ps` 开例外，
> 打印环境变量的写法在权限门询问”来做（另一种是允许按程序名把 `sandbox_escalate(ps *)` 记到项目，被否：
> 仍要每个项目问一次，记住后整条命令都出沙箱）。

## 一、为什么

macOS 的 `/bin/ps` 是 setuid root，沙箱里的进程不能执行 setuid 程序，`ps` 一律报
`operation not permitted`，日志里没有带标记的事件，只能走整个脱沙箱。`ps -axo …` 第二个词是选项，前缀
不稳定，只能“允许一次”，每次用都问。plan 283 阶段 4 的数据里出现 1 次。参考实现（codex）的沙箱同样跑不了 `ps`。

## 二、做法

- profile 新增一段 `seatbelt_ps.sbpl`，只有一条规则：`(allow process-exec (literal "/bin/ps") (with no-sandbox))`。
  实测加上后沙箱里的 `ps` 正常输出，只有 `ps` 这个进程不受沙箱约束，管道后面的命令照旧在沙箱里。
  `pgrep` 在沙箱里仍用不了（它找系统服务 sysmond，profile 不放），不替代。
- 权限门：能打印别的进程环境变量的 `ps` 是新的安全检查 `[process environment]`，每次都问。认的写法：选项簇里
  的 `E`；BSD 写法（第一个参数不带 `-`）里的 `e`/`E`；认不出的参数（变量、命令替换）。带值选项（`-o`、`-O`、
  `-G`、`-g`、`-u`、`-p`、`-t`、`-U`）的值不算。直接执行、剥掉包装器（`sudo`、`timeout`）、循环绑定都看。
  legacy 模式（`COMMAND_MODE=legacy`）下的 `-e` 不认。只在 macOS 上生效。
- 命令行不另行保护：本机任何用户在沙箱外本来就能看到所有进程的命令行。

## 三、验收

- 真实 seatbelt：`ps -p $$ -o pid= | head -1` 在沙箱内成功，不触发升级。
- `ps -E`、`ps -axE`、`ps eww`、`ps auxe`、`sudo ps -E`、`ps $FLAGS`、循环绑定出 `-E` 都是
  `process environment`；`ps aux`、`ps -ef`、`ps -o etime -p 1`、`ps -Oe`、`ps -p $PID -o pid=` 不是。
- manual 下普通 `ps … | grep … | head` 不问；`ps -E -p 1` 问一次。

## 四、结果（2026-11-19）

- 以上全部实现；profile 的整段比对用例加上了新段。
- **验证**：新增 `plan288_*` 两个用例和真实 seatbelt 用例 `ps_runs_inside_the_sandbox`；kloop-core 库测试在
  执行器沙箱外 1475 过、1 ignored；`cargo fmt --check` 与 `cargo clippy -p kloop-core --all-targets
  -D warnings` 干净。加规则前后的差别是手工在 kloop profile 里验证的（没有规则时
  `operation not permitted: ps`，有规则时正常输出）。回放读的是原结果里记下的拒绝，看不出这一条。没跑 `make check`。
