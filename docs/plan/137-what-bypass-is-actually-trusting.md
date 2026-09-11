# Plan 137 — bypass 到底在信任什么

> 来源:2026-09-11,plan 136 收尾时把 `argv_is_dangerous` 只认 `rm`/`sudo` 挂成"等用户拍板"。
> 用户问「这个有什么需要拍的?」——问得对,"要不要多列几个命令"确实不值得拍。摆出三条路
> 之后用户选 **(c)**:bypass 下不看命令黑名单,看 containment。

## 一、扩表是一条追不完的路

`argv_is_dangerous` 是**黑名单**,而 HANDOFF 教训 8 立的规矩是:

> 凡是"分类后放行"的逻辑,分类器必须是白名单而非黑名单。

那条教训当时是为 `analyze_bash` 立的(解析不了 = 不可分析 = 永不自动放行),但
`argv_is_dangerous` 是同一类判定的另一半,方向却是反的。`dd` / `mkfs` / `shred` /
`git clean -fdx` 之后还有 `chown -R`、`curl | sh`、`git push --force`、`> /dev/sda`……
列不完,而列不完的部分**全部静默放行**。往表里加几个词的真实收益是挡住几个高频误操作,
真实代价是把一个黑名单正当化——下一个读者会以为这张表是穷尽的。

所以要换的不是表,是**放行的理由**。

## 二、走到 bypass 层的 bash,有三种来路

沙箱 auto-allow 是第 6 层,bypass 是第 7 层。一条 bash 走到第 7 层,只能是这三种之一
(`sandbox_auto_allowed` = `bash` 且 `call_sandbox(...)` 有 policy 且 `policy.auto_allow`):

| 来路 | 有 containment 吗 | 谁做的决定 |
|---|---|---|
| **A. `disable_sandbox: true`** | **没有** | **模型,就在这次调用里** |
| B. `workspace.sandbox` 是 None(沙箱关闭 / 平台不支持) | 没有 | 用户,在会话开始前 |
| C. `policy.auto_allow == false` | 有 | 用户,在配置里 |

现状是三种一视同仁地放行(opaque 脚本除外)。但它们的含义完全不同,**本轮只收紧 A**:

- **A 是模型自己解除保护。** bypass 的语义是"我信任你要做的事",不是"我信任你自己拆掉
  护栏"。而且 `describe_parts` 早就为它准备了 notice(`no OS sandbox — full filesystem
  and network access`)——设计上一直认为这件事值得说出来,只是 bypass 让它永远说不出口。
- **B 收紧等于重新定义 bypass。** 在 Windows 或关掉沙箱的环境里,B 是**每一条** bash 的
  来路;收紧它会让 bypass 在一整类环境里对 bash 完全失效。那是换掉这个模式,不是补一个洞。
- **C 是用户的两条配置互相矛盾**(`auto_allow = false` 说"审批照旧",bypass 说"别问"),
  现状是 bypass 赢,而这正是 bypass 的定义。改它是在裁决两个配置谁优先,单独想。

## 三、改法

第 7 层的放行条件从"命令不是 opaque"变成"命令不是 opaque **且这次调用没有主动放弃
containment**"。`CallFacts` 记一个 `escapes_sandbox`(`bash` + `disable_sandbox: true`),
opaque 脚本原有的 `no_sandbox` 复用它,不再各算一遍。

被挡下来的调用**不是被拒**,是落到下面的层继续判:

- 第 8 层只读自判:一条只读命令即使脱沙箱也无害(读敏感路径在第 2 层就被硬挡),**放行**;
- 第 10/11 层 allow 规则与会话缓存:命中就放行(见第五节的备查);
- 第 12 层:问用户,notice 里带着那句 `no OS sandbox`。

净效果:**bypass + 主动脱沙箱 + 非只读 + 没有规则兜底 → 问一次**。其余一切不变。

## 四、非目标

- **不扩 `argv_is_dangerous`。** 本轮换掉的是 bypass 这条路径上的放行理由;那张表在来路
  B(无沙箱环境)下仍是唯一的网,仍只认 `rm`。这个事实本轮**写进模块头**,让它成为一句写下来
  的话,而不是要读代码才发现的东西。扩不扩表之后再单独决定。
- **不动来路 B 与 C**(见第二节)。
- **不给 bash 的 allow 规则/会话签名编码 `disable_sandbox`。** plan 135 的非目标里已经记过
  这条既有行为:`bash:go test` 这类前缀签名没编码逃逸标志,所以"沙箱内批准过的 `go test`"
  理论上能覆盖一次脱沙箱的 `go test ./x`。**本轮会扩大它的可达范围**——原本只在
  `auto_allow = false` 的沙箱环境里够得着,改后 bypass + `disable_sandbox` 也会走到那一层。
  仍不在本轮修:动它要重新定义 bash 前缀规则的含义,与 plan 129/135 建立的
  `bash(...)` / `sandbox_escalate(...)` 之分是同一个题目,应该一起做。

## 五、第二轮:那张表还是收了三个(第四节第一条被推翻)

第一轮落地后用户问「argv_is_dangerous 去掉了吗」——没有,它在**第 4 层**(safety checks,
bypass 免疫),而本轮换的是**第 7 层**的放行理由。两层方向相反:第 4 层命中就问,第 7 层
不命中才放行。去掉它会让 bypass 下的 `rm -rf /` 直接跑,那是净损失。**不完备 ≠ 没用**:
它覆盖到的那一小块是真的在挡,而且正是在最该挡的模式下挡。

摆明这一点之后,扩表的成本已经变了:bypass 的主要放行理由换成了 containment,表的局限也
写进了模块头和 README,所以"再往里加几个词"不再有"让人以为这表是穷尽的"的风险。用户
**同意扩**,于是第四节第一条推翻。

加什么由**一条规则**决定,不是由"这命令读起来多吓人"决定:

> **不可逆,而且 git 不是回去的路。**

按这条规则进来的:`dd` 带 `of=`(bytes 落在哪里,原来的就没了;不带 `of=` 只是读,和 `cat`
一样无害)、`mkfs*`(每一种拼写都是格式化,与 flag 无关)、`shred`(就地覆写是它的用途)。
按这条规则**留在外面**的:`git clean -fdx` 和 `git reset --hard`——在一个仓库里它们是恢复
手段,kloop 自己就在用;把它们变成提示是"blocklist 靠感觉长大"的典型。`chmod -R` /
`chown -R` 也没收:它们破坏的是权限而不是数据,而且改得回去。

`… > /dev/sda` 这类**不需要**条目:重定向让整条脚本变成 `BashAnalysis::Opaque`,而 opaque
在任何地方都拿不到自动裁决(第 7 层的另一半条件)。测试里把这一点也断言了,免得下一个人
为它再加一行。

## 六、验收

- bypass + `disable_sandbox` + 非只读 bash → **问**,且 notice 含 `no OS sandbox`;
- bypass + 普通 bash + 无沙箱环境(来路 B)→ 仍然直接放行,不回归;
- bypass + `disable_sandbox` + 只读 bash → 第 8 层放行(脱沙箱的 `ls` 不值得问);
- bypass + 沙箱 `auto_allow` 开着 + 普通 bash → 第 6 层放行,根本到不了第 7 层;
- 非 bypass 模式下的既有行为一条不变。

## ✅ 已完成(2026-09-11;提交 SHA 以本条所在提交为准)

`crates/core/src/permissions.rs`:

- `CallFacts` 新增 `escapes_sandbox`(`bash` + `disable_sandbox: true`),`OpaqueScript.no_sandbox`
  复用它而不再各算一遍;
- 第 7 层放行条件加上 `!call.escapes_sandbox`,并把第二节那张"三种来路"的表写进层内注释
  ——包括**为什么 B 和 C 不收紧**,以及最后一段明说"这一层不审命令,`argv_is_dangerous` 是
  只认 `rm`/`sudo` 的黑名单,没有沙箱的主机上 bypass 就没有命令级的网";
- 模块头补同一件事的摘要。

`README.md`:`--permission-mode bypass` 的一句话描述,以及 sandbox 段落里
"bypasses approvals but not the sandbox"那句后面补上"也不包括自己拆掉沙箱的那一次调用",
连同只读例外、无沙箱主机不受影响、以及"没有 containment 就没有命令级的网"这个立场。

**行为变化只有一处**:`--permission-mode bypass` 下,一条**非只读**、**主动带
`disable_sandbox: true`** 且没有被 allow 规则/会话缓存覆盖的 bash 调用,现在会问一次,
notice 里带着 `no OS sandbox — full filesystem and network access`。其余一切不变。

### 测试

`bypass_stops_at_a_call_that_gave_up_containment` 一条走完第六节的五项验收:脱沙箱的
`make install` 被问到并带对 notice;同一模式同一(无沙箱)会话里普通 `make install` 照旧直接
跑;脱沙箱但只读的 `ls -la` 由第 8 层放行;`check_call(..., sandbox_auto_allow = true)` 的
`make install` 在第 6 层就过了、根本到不了这一层;manual 模式的行为与脱不脱沙箱无关。
**反向验证过**:把 `!call.escapes_sandbox` 去掉,第一条断言立刻红。

**第二轮(第五节)**:`crates/core/src/shell.rs` 的 `argv_is_dangerous` 收入 `dd of=`、
`mkfs*`、`shred`,并把"不可逆且 git 不是回去的路"这条选择规则连同两个**反例**
(`git clean -fdx` / `git reset --hard`)写进函数文档;`permissions.rs` 的模块头与第 7 层
注释、README 的危险分类器段落同步。

- `shell.rs` 一条:`the_danger_list_holds_only_irreversible_loss_git_cannot_undo`——三个新
  条目的正例(含 `sudo dd of=`、`/sbin/mkfs.xfs` 这种带路径的拼写)、`dd if=` 不带目的地的
  反例、两个 git 命令的反例,外加断言 `cat x > /dev/sda` 是 `Opaque`(所以不需要条目)。
- `permissions.rs` 一条:`irreversible_commands_reach_the_bypass_immune_layer`——三个新条目
  在 **bypass** 下都被问到且带 `[destructive]` 标记,而 `dd if=`、`git clean -fdx`、
  `git reset --hard` 在同一模式下照旧直接跑。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
`cargo test --workspace` 各自单独跑、当场取退出码,依次 0 / 0 / 0(两轮各跑一次)。
