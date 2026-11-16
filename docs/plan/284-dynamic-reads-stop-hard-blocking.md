# Plan 284 — 带变量的命令不再一律硬拒 ✅

> 2026-11-16 立并完成。来源：用户在另一个项目里让 kloop 跑 MySQL 集成测试，命令形如
> `DB=x_test_$(date +%s) && mysql … -e "CREATE DATABASE $DB" && … go test …`，被报
> “reading this sensitive path is blocked. Do not retry or work around the protection.”
> 模型以为碰了凭据，直接停手让用户自己跑。用户：“太严了，影响工作了。”

## 一、原因

plan 244 第六轮把“未决读取”（`ReadVerdict::Unresolved`）和明确敏感读取放进同一道硬拒：
命令不在参数角色表里（`mysql`、`go`、`curl`、`gofmt`、`cat`…）而参数含变量或命令替换，
或已知命令的读取位置是未知值，都在 deny 之后、审批/沙箱/bypass 之前拒绝。日常带变量的
命令几乎全中；报错又说成“敏感路径”，模型连把变量改成字面值都不试。

这道硬拒还兼着别的防线，不能整条拿掉：
- 执行体未知：`$CMD file`、包装器无法定位（`env --未知选项 … bash -c`、`sudo -R`）、
  动态 `-c` 正文、xargs 替换（plan 243/272/281）；
- deny/ask 规则：部分命令只按第一个未知词之前的字面前缀匹配，`git $ACTION push origin`
  对 `bash(git push *)` 不命中，靠这道硬拒兜底（plan 239）。

## 二、决定（用户确认）

- 明确拼出的敏感路径照旧硬拒。
- 命令名与包装器都已定位、只是参数值未知的读取不再硬拒：沙箱内照常自动放行（OS 沙箱运行时
  挡 `~/.ssh`、`~/.gnupg`、`~/.aws` 与 `~/.kloop`），无沙箱走普通审批，bypass 直接执行。
- 接受的缺口：沙箱运行时不挡 `.env*` 与 rc 文件；变量在运行时恰好展开成它们时不再静态拦截。
  用户：“可以接受，目标是不能影响工作，尽量不打扰用户。”

## 三、实现

- `bash_reads_sensitive_path`：原文命中敏感目录直接判 `Sensitive`；逐命令/重定向扫描时未决
  只记一笔、不再提前返回（否则 `mysql -e "$X"; cat .e*` 会漏掉后面的明确敏感读取）。
  扫完后未决在两种情况下仍升级为 `Sensitive`：`BashFacts::reliable` 为假（执行体未知），
  或原文按 shell 分隔符与 `/` 切词后有凭据文件名（`names_secret_file`）。
- `ReadVerdict::blocked` 只认 `Sensitive`；`bash_protection_requirement` 把未决当作无需保护。
- `CallFacts::unresolved_read`：deny/ask 对这类调用按“可能命中”匹配（`rule.rs` 的
  `prefix_may_match`）：未知词可能展开成任意个词，已知前缀（syntax 视图及每层包装器视图）
  与规则在公共长度上一致即命中，精确规则另要求前缀不长于规则。只用于未决读取的调用，
  所以 `rm "$f"` 这类原本就放行的调用不新增拒绝。

## 四、验证

- 新增 `permissions/plan284_tests.rs` 五组：日常动态命令在 manual/bypass 沙箱内不问不拒；
  无沙箱时问一次；明确敏感读取与同命令凭据文件名照旧硬拒（含未决在前的扫描顺序）；
  正文凭据探针旁加变量参数仍判 `Protection::Required` 并在沙箱内；deny 不被未知词绕过、
  `rm "$f"` 不新增拒绝。
- 改写 plan 239/243/244 与 `permissions.rs` 里钉住“动态即硬拒”的断言：已定位命令的动态
  参数改为放行或普通审批，执行体未知与凭据文件名的用例保留硬拒；`git $ACTION push` 改为
  断言 deny 规则命中。
- `cargo test -p kloop-core --lib permissions::` 208 项通过（1 ignored）；`tools::bash` 81 项
  通过，其中 29 项真实 seatbelt 用例实际运行（未跳过）；`cargo fmt --check`、
  `cargo clippy -p kloop-core --all-targets -D warnings` 通过。未跑全量 `make check`、parity，
  Linux/Windows 未实测。

## 五、复审修复 ✅（2026-11-16）

用户指出：`bash_reads_sensitive_path` 把正文保护与未决读取压进一个返回值，两者同时成立时
只剩 `RequiresProtection`，`CallFacts::unresolved_read` 为假，deny/ask 的可能命中被跳过。
带凭据保护的 macOS 沙箱里配 `deny = ["bash(make clean)"]`：单独的
`TARGET=clean; make "$TARGET"` 命中禁令，前面加一段点名 `~/.ssh/id_rsa` 的 heredoc 后不再
命中，ask 规则同样漏。

修法：`ReadVerdict` 只留单条 argv/重定向的三态（Clear/Sensitive/Unresolved）；整条命令的
判定改为 `BashRead::{Blocked, Allowed { requires_protection, unresolved }}`，两个标记各自
保留。`unresolved_read` 只看 `unresolved`，`bash_protection_requirement` 只看
`requires_protection`。

回归 `plan284_literal_body_protection_keeps_the_unresolved_read_for_rules`（macOS）：有无
正文两种形状都命中 deny（bypass）并触发 ask（manual，批准后仍在沙箱内），正文那条另断言
`Protection::Required`；修复前带正文的一条通过了 deny。验证：`permissions::` 209 项通过
（1 ignored）、`tools::bash` 81 项、`tools::scheduler` 4 项、fmt、
`clippy -p kloop-core --all-targets -D warnings`；未跑全量 `make check`、parity。
