# Plan 144 — 一个重定向 token,把整条脚本拖下水

> 来源:2026-09-14,用户看着自己的
> `~/.kloop/projects/v1/p1_434f…/permissions.json` 说:
> 「加了很多的 bash_script_no_sandbox,都命不中,完全没意义了。」

## 一、这不是"命中率低",是"一次都不可能命中"

那份文件的实测(45 条 allow):

| | 条数 | 字节 |
|---|---|---|
| `bash_script_no_sandbox(<原文>)` | **38** | 13.2 KB / 14.4 KB |
| 其余(`bash(go test *)`、`write_file(...)` 等) | 7 | 1.2 KB |

38 条**互不相同**。按"为什么不可解析"分类:

- **29 条唯一的原因就是末尾的 `2>&1`**;
- 3 条是 `2>&1` 再加 `$?` / 变量;
- 只有 6 条真的带 heredoc / 写重定向(`cat > main.go <<'EOF'`),那种脚本确实一次性。

而同一份文件里**早就有** `bash(go test *)` 和 `sandbox_escalate(go test *)`。那 29 条如果
能解析,会拆成 `cd` / `go test …` / `tail|grep` 三段,`cd`、`tail`、`grep` 是只读段自动
放行(`allow_rules_match` 的 `argv_is_readonly ||`),只剩 `go test` 段——**被已有规则直接
覆盖**。也就是说这 29 次询问本来一次都不该弹,更不该落成 29 条死规则。

plan 135 自己写着「把它变成 opaque 的是末尾的 `2>&1`」,但那一轮修的是**记不住**(补上
逐字规则),没有修**为什么记不住**。逐字规则对真实用法的复用率是 0:`-run` 的正则、包
列表、`tail -3/-5` 每次都不一样。

## 二、根因

`shell.rs` 的白名单遍历 `word_only_commands_sequence`:`ALLOWED_KINDS` 没有
`redirected_statement` / `file_redirect`,`ALLOWED_PUNCT_TOKENS` 只有 `&& || ; | " '`。
重定向节点一到,整条脚本判 `Opaque`;`permissions.rs` 的 opaque 分支只能按**原文全等**
匹配(`Rule::allows_script`)。

## 三、做什么

放行**不产生 argv 看不见的副作用**的那一类重定向,其余原样 Opaque。判据一句话:
**这个重定向会不会写出/读入一个 argv 里没有的文件**。

| 形态 | 判定 | 理由 |
|---|---|---|
| `2>&1`、`>&2`、`1>&2` | **可解析** | 只是把一个流接到另一个描述符上 |
| `>/dev/null`、`2>/dev/null`、`&>/dev/null` | **可解析** | 目标是丢弃设备,没有文件产生 |
| `> f`、`>> f`、`&> f`、`>| f` | Opaque | argv 里看不见的写 |
| `< f`、heredoc、herestring | Opaque | argv 里看不见的读/内容 |

重定向本身不进 argv(它在 CST 里是 `command` 的兄弟,不是孩子),所以 `go test ./x 2>&1`
的 argv 就是 `go test ./x`,前缀记忆照旧落 `bash(go test *)`。

连带生效的(都是同一个解析结果喂出去的,属于修好之后的正确行为,不是额外放宽):
`bash_reads_sensitive_path` 从"只扫裸串"升级成能逐参数查敏感路径;`argv_is_dangerous`
能看见 `rm -rf x 2>&1` 里的 `rm`;只读流水线可以并发(`Builtin::concurrency_safe`);
bypass 模式不再为一个 `2>&1` 停下来问。

## 四、非目标

- **不动 opaque 的逐字持久规则**(plan 135 第四节是用户当场拍板要的)。修完之后还会落到
  那条路上的,只剩 heredoc 探针那一类,确实一次性——要不要退回会话级,修完再问。
- **不动 PowerShell**(plan 135 同一条非目标,理由不变:它压根没有分析器)。
- **不放宽 `<` 输入重定向**。它把一个 argv 里没有的文件喂进命令,和写一样要看得见。
- **不为已落盘的死规则做迁移代码**。那 38 条直接从用户的 `permissions.json` 里删掉。

## ✅ 已完成(2026-09-15;提交 SHA 以本条所在提交为准)

`crates/core/src/shell.rs`:白名单加 `redirected_statement`,新 `redirect_is_stream_only`
逐个审 `file_redirect`——**判定看操作符文本,不看树的形状**,因为 `foo > 1`(写一个叫 `1`
的文件)和 `2>&1` 解析出来是同一个 `destination: (number)`。放行两类:`>&` / `<&` 且目标是
纯数字(fd 复制),`>` `>>` `&>` `&>>` 且目标恰好是 `/dev/null`(丢弃)。重定向节点判完整个
跳过,**不往下走**:它的操作符 token 不该进 `ALLOWED_PUNCT_TOKENS`(那会让 `>` 在脚本任何
位置都合法),它的目标也不是参数。模块头与 `argv_is_dangerous` 的"重定向 → Opaque"两处
注释改成"重定向到**文件** → Opaque"。

`crates/core/src/permissions.rs`:plan 135 的三个 fixture **全部靠 `2>&1` 才 opaque**,本轮
换成真正的写重定向(`> out.log` / `> merged.log`),测的行为一字未动——这本身就是证据:当
时对"opaque 脚本长什么样"的印象,是被这个 bug 塑造出来的。

README 六处:解析规则段(新增 stream-only 例外 + `go test ./x 2>&1 | tail -3` 的例子)、
设备重定向段、Asking 段的"什么叫写不成前缀"(`go test … 2>&1` → `cat > probe.go <<'EOF'`)、
并发批处理段、沙箱 auto_allow 段、测试索引。

### 顺带修掉的一个洞(测试变红才发现)

`opaque_bash_is_not_auto_run_in_bypass` 的 fixture 正是 `rm -rf build > /dev/null`。改完之后
它不再是"opaque → 落到人"那条路:命令被解析出来,`bash(rm *)` 这条 **deny 规则直接命中**,
一个人都不用问。原测试断的 `ask_count == 1` 因此变红。两条都留着:`> cleanup.log` 那条继续
守"写文件仍然 opaque、bypass 下也必须问人",`> /dev/null` 那条新断"解析出来之后连问都不用
问"。**这条 gate 因此是被收紧的,不是被放宽的**——同一个洞在 plan 137 的 bypass 层被堵过
一次(那次让 opaque 不再自动跑),这次是把"为什么它当初是 opaque"也拿掉了。

### 测试

- `shell.rs`:`stream_only_redirects_leave_the_script_parseable`(`cd sub && go test ./pkg
  -count=1 2>&1 | tail -3` 拆成三段 argv、重定向不进 argv;`>/dev/null`、`2>/dev/null`、
  `>&2`、`&>/dev/null`)与 `a_redirect_that_touches_a_file_still_sinks_the_script`
  (`>>`、`<`、`&> log`、**`foo > 1`**、`2>&1 > out` 一条坏的就够、`3>&-`、heredoc、herestring)。
- `permissions.rs`:`a_stream_only_redirect_keeps_the_prefix_rule_working`——带 `2>&1` 的
  `go test` 被既有 `bash(go test *)` 覆盖、**零次询问**;没有规则时 `p` 落的是
  `bash(go test *)` 而不是整条命令原文。
- `opaque_bash_is_not_auto_run_in_bypass` 按上面拆成两条断言。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、
`cargo test --workspace`,各自单独跑并当场取退出码(HANDOFF 111(b2))。

### 用户那份 permissions.json

38 条死规则已删(45 → 7 条,14.4 KB → 0.5 KB),`revision` 45 → 46,原文件备份在本次会话的
scratchpad。留下的 7 条里有两条 `sandbox_escalate(cd /tmp/gateway-…-<随机后缀> *)` 同样
是一次性的,但它们是用户当时按前缀批的,不属于本轮清理范围。
