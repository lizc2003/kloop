# Plan 288 — `ps` 在沙箱里能跑；打印进程环境变量的 `ps` 先问（已回退）

> 2026-11-19 立并完成，同日因复审回退，见第五节。来源：plan 283 第十三节后续候选第 6 条。用户同意按“profile 只给 `/bin/ps` 开例外，
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

## 五、复审回退（2026-11-19）

**问题**（P1，核实成立）：SBPL 的 `process-exec` 规则只认被执行的文件，不认是谁执行。profile 里的例外对沙箱内
所有后代进程都生效：权限门只看提交给 bash 工具的命令和它能解析的包装器，一条正常放行的 `cargo test`、
`make` 跑起来的仓库脚本里再调用 `ps -E`，就能不经询问地把别的进程启动时的环境变量带回工具输出。
profile 层面没有办法按参数限制，所以 `[process environment]` 检查管不到这些后代。

**处理**：代码全部回退（`seatbelt_ps.sbpl`、profile 段、`ps` 参数判定与 `[process environment]` 检查、三个
测试），`ps` 回到在沙箱里执行不了、走整个脱沙箱的状态。保留例外、只靠权限门检查，管不住后代进程；不要例外、
只留检查也没有意义：没有例外时 `ps -E` 本来就会走到整个脱沙箱的询问，用户看得到命令。

**之后**：复审给的方向是“免审批的进程查询应由 kloop 直接启动并限制参数”，即不放宽沙箱，而是 kloop 在核对过
参数后自己在沙箱外启动 `/bin/ps`。“整条管道放到沙箱外”不成立：bash 子进程的 PATH 来自登录环境，门那一刻
确定不了 `ps`、`grep` 解析到哪个文件。可行的做法是只认 `ps …` 开头的单条管道，kloop 用绝对路径单独启动
`/bin/ps`，管道其余部分仍在沙箱里、经临时文件接上它的输出。**不做**（2026-11-20 用户定）：这要改 bash 工具
最复杂的执行路径，而一周数据里 `ps` 只引起 1 次询问。

**验证**：回退后 kloop-core 库测试与回退前（plan 287 之后）一致，见提交信息。
