# Plan 166 — 别人的审查清单,只留门禁抓不到的

> 来源:2026-09-18,用户丢来 `https://github.com/alibaba/open-code-review`(阿里开源的
> AI 代码审查 CLI,Go,Apache-2.0)问"对项目是否有帮助"。调研结论:**工具本身不装**
> —— 它的主战场是 MR/PR 门禁 + 行级回帖,kloop 是单人直推 main、没有评审环节,那一整面
> 用不上;本地 `ocr review` 又和现有这条路重复。**值得拿的只有一样**:
> `internal/config/rules/rule_docs/rust.md`,一份成体系的 Rust 缺陷清单。
> 用户拍「摘成 .kloop/skills 技能」。当场做完(✅ 见文末)。

## 现状

- kloop 已有 builtin `code-review`(plan 119),写的全是**方法**:什么算 finding、每个候选
  都要落一个裁决、怎么就地验证、报告怎么写。**一条语言专项的"该看什么"都没有**。
- CI 跑 `cargo clippy --workspace --all-targets --all-features -- -D warnings`。
- 上游 rust.md 61 行 10 节,通用 Rust 审查规则,没有任何 kloop 语境。

## 裁决

### 一、只收 clippy 抓不到的

builtin `code-review` 的排除项里明写"编译器、linter、类型检查、formatter 能抓的不算
finding",而 CI 的 clippy 是 `-D warnings`。两条叠起来的结论是:**任何默认 lint 覆盖的条目
写进清单就是纯噪音**——它根本走不到审查这一步,CI 先红。于是逐条过筛:

- **删**:拼写错误一节(价值低,且 code-review 的 nitpick 排除项已经盖住)。
- **改写而不照抄**:"锁跨 await"整条不能抄——`await_holding_lock` /
  `await_holding_refcell_ref` 是默认 warn 的。只留它看不见的那一面:`tokio::sync` 的 guard
  持有过久、`std` guard 跨长同步段、guard 跨调用用户代码或可能再取锁的调用。
- **保留,但在条目里写清那条 lint 为什么不管**:`unwrap_used`/`expect_used` 是
  restriction、**故意不开**(记录不变量的 `expect` 是对的,只有人能分辨两者);
  `redundant_clone` 在 nursery;`cast_possible_truncation` 在 pedantic;
  `undocumented_unsafe_blocks` 在 restriction。

这条约束写进了技能正文,免得下次有人往里补条目时把 clippy 的活抄回来。

### 二、加一节 kloop 自己的复发点

上游清单是通用的。kloop 近两个月真实修过的缺陷集中在它没有的一处——**与外部流的契约**。
四条,都来自已归档的 plan,写成审查项而不是事故回顾:

- 校验断言"这个形状不可能",而产生它的恰恰是我们自己;同一个校验函数还管落盘,收紧它
  不只是拒掉一轮,是让已写出的会话打不开(plan 164)。
- 同一份数据到达两遍:覆盖、累加、报错是三种不同的 bug;没有裁判依据时累加会把计数翻倍,
  而预算和 `/cost` 都读它(plan 163)。
- fail-closed 对错了人:该 fail-closed 的是我们自己保证不了的不变量,对方犯的错要回给对方,
  连同它写的原文(plan 165 / 教训 161)。
- 新变体能不能不进磁盘:能在落库前规范化掉,影响面就只剩一段流(教训 161③、教训 7)。

另外三条散落别处的也收进来:取消路径没有 cancel token 时要兜底丢 future(plan 151)、
重试的退避/超时/上限/取消传播四件套要齐(plan 154 同源)、分类后放行的分类器必须是白名单
(教训 8)。

### 三、放 `.kloop/skills/`,不做 builtin ⤴ 当天作废(见文末)

用户指定。代价记在这里:发现是 **cwd 相对**的(`startup.rs::load_skills` 只看
`cwd/.kloop/skills` 和 `~/.kloop/skills`,**不向上找项目根**),所以**在仓库根起的会话才看得到
它**;习惯在 `rust/` 下起 kloop 的话这个技能不存在。要覆盖全目录只有两条路:做成 builtin
(与 `code-review` 并列,编译进二进制),或者拷一份进 `~/.kloop/skills/`。

### 四、`.gitignore` 要开一个洞,而且不能顺手改 ⤴ 当天回滚(见文末)

`.kloop/` 这条规则**内部不含斜杠,所以匹配任意层级**的同名目录——agent 在哪个 cwd 起就在
哪里写一个,`rust/crates/core/src/.kloop/` 就是这么被盖住的。第一版为了开洞把它改成
`.kloop/*`,规则当场变成**锚定在仓库根**,其它层级的运行时目录全部泄漏出来(`git status`
里立刻多了一行)。

正解是保留原规则,再补三行:git 不会进入被排除的目录,所以要先把根上那个 `.kloop` 目录本身
放回来,再排除它的内容,最后才谈得上 negate `skills/`:

```
.kloop/
!/.kloop/
/.kloop/*
!/.kloop/skills/
```

## 验收(首版;三、四两条已于当天回滚,见文末)

- `git check-ignore`:`.kloop/env.local`、`.kloop/worktrees/x`、`rust/.kloop/a`、
  `rust/crates/core/src/.kloop/a`、`docs/.kloop/a` 全部仍被忽略;
  `.kloop/skills/rust-review/SKILL.md` 可跟踪。
- 新测试 `startup::tests::the_repositorys_own_skills_all_load`:走真实发现路径
  (`skills_from_roots`)加载仓库自带的 `.kloop/skills`,warnings 必须为空。
  **negative control 做过**:把 description 里插一个 `: `,测试报
  `invalid frontmatter: mapping values are not allowed in this context`。

## ⤴ 当天移出仓库

接着做了 `go-review`(上游 `rule_docs/go.md`,同一套筛法)。它把位置这件事戳穿了:
**kloop 仓库里没有一行 Go 代码,而技能发现是 cwd 相对的**——放在 `.kloop/skills/` 的 Go 清单
永远不会出现在有 Go 代码的地方。于是 go-review 直接放进 `~/.kloop/skills/`,用户随后要求
rust-review 也搬过去。两份现在并排在那里,任何目录起的 kloop 都能用。

内容层面的裁决(一、二)不受影响;另外三处全部回滚:

- `.gitignore` 的四行洞回到原样的 `.kloop/`。那条教训仍然是真的(HANDOFF 教训 162),
  只是这个仓库暂时用不上它。
- `startup::tests::the_repositorys_own_skills_all_load` 删除。仓库里不再有自带技能,
  它只会走 `!root.is_dir()` 那条静默 return,是一条永远空跑的测试。
- **代价要认**:两份技能不在版本控制里,换机器丢失;编辑时写坏 frontmatter 也没有任何东西
  会发现(一个裸 `: ` 就够,见教训 162)。搬迁前的验证办法是把技能拷进仓库 `.kloop/skills/`
  跑一次那条测试——测试删掉之后,只剩"起一次 kloop 看有没有 skip 警告"。
  要同时拿回版本控制和全目录可用,只有一条路:做成 builtin
  (`core/src/skills/`,与 `code-review` 并列注册进 `BUILTIN_SKILLS`)。

## ✅ 完成

2026-09-18 完成,一次提交 `923f4f8`。
