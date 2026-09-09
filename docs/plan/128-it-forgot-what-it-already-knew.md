# Plan 128 — 压缩之后，它不知道自己已经知道什么

> 来源：2026-09-09 第七轮三方对照。用户在同一个会话里连发四个审查任务
> （`2bfa0db1` → `f11e88ca` → `e1f59ef4` → `6de3a0e2`，一条前后相继的修复链），
> 同时交给 kloop 与 codex（claude 这轮是**被审对象**：它审 `2bfa0db1` 之后写的修复
> 就是 `f11e88ca`）。kloop 跑的是 plan 126 + 127 全部落地后的版本。
>
> **对照条件已核实**：同 provider（`gw_router`）、同模型（`gpt-5.6-sol`）、同
> effort（`xhigh`）、同沙箱语义。kloop 是最新版有行为证据：rollout 里
> `a remembered sandbox_escalate rule already covers` 出现 5 次——那是 `70d71de`
> 才有的文本，同时证明 plan 127 的记忆前置真的跳过了 5 次沙箱内执行。
>
> **本片的硬约束（用户，2026-09-09）**：「不要让审查变慢，受不了漫长的审查」、
> 「可以忍受漏一些无关紧要的，比如文档什么的，还是要想尽办法提高审查的速度」。
> 本片只收**降本**的改动。

## 一、plan 126 / 127 的复查：基线达成

| | 基线 `d9952d70` | 现在 `f11e88ca` |
|---|---|---|
| **工具段** | **20 min** | **0.7 min** |
| 最慢单次工具 | 556s | 29s |
| 记忆命中跳过沙箱 | — | 5 |
| 建临时树 | 0 | 0 |

工具段塌了 28 倍。**顺带印证**：codex 在 `6de3a0e2` 的验证里如实写了
「`go test ./upstream/<pkg>` 因沙箱禁止 `httptest` 绑定 IPv6 端口而失败」——
plan 127 第一节「真凶是网络不是文件写」的结论，在对手身上又复现了一次。

## 二、四个任务：「kloop 更慢」不成立，但有一个异常值

| 任务 | kloop | codex | kloop 轮 | codex 轮 | 压缩 |
|---|---|---|---|---|---|
| `2bfa0db1` | 24.6m | 21.2m | 17 | 45 | — |
| `f11e88ca` | **27.6m** | **10.0m** | 60 | 20 | kloop 1 次 |
| `e1f59ef4` | **13.3m** | **22.7m** | 12 | 56 | **codex 1 次** |
| `6de3a0e2` | 5.7m | 5.3m | 5 | 17 | — |

赢一、平二、输一，kloop 轮数普遍更少。**唯一压缩过的任务，就是唯一大幅落后的任务**
——两边都是（codex 在 `e1f59ef4` 上 56 轮、同样压缩）。

## 三、根因：压缩打断的是批处理，不是记忆本身

`f11e88ca` 以压缩点为界劈开：

| | 轮数 | 单工具轮 | 平均工具/轮 |
|---|---|---|---|
| 压缩**前** | 13 | 46% | **3.6** |
| 压缩**后** | 45 | **87%** | **1.5** |

同一个任务、同一套工具、同一个模型，压缩之后每轮只发 1.5 个工具调用，87% 的轮次
只发**一个**。压缩前 13 轮就走完了大半，压缩后又花了 45 轮。

每一个单工具轮都要付一次完整的 TTFT 加约 677 tokens 的思考——而输出的 82–95% 是
reasoning（可见文本只占 5–18%，`f11e88ca` 全轮加起来才 3,956 字符）。**所以「让它
少写进度叙述」这条路是死的，砍光也省不到一分钟；轮数才是唯一的乘数。**

模型不是忘了要批处理，是**不知道自己已经知道什么**。摘要保住了做过什么动作，没保住
那些动作证明了什么，于是它只能一次一个地重新试探。

### 排除掉的方向（都做过测量）

- **工具粒度**：每轮动作数 kloop 1.9、codex 2.05，几乎相同。kloop 一轮本来就能并发
  多个 `read_file`（`6de3a0e2` 每轮 3–7 个），`read_file` 支持多区间/多文件省下的只是
  几个 tool_use 块的 token，**不减少轮数**。
- **让模型多用 bash 组合命令**：~~与 plan 98 冲突~~ ——**这条排除是错的，见第三之二节
  末尾**。实测反过来支持它：最快那轮的开局就是一条 bash 组合命令。
- **新工具**：没有数据指出缺哪个；`git diff -U<n>` 这个能力本来就在 bash 里。
- **降 effort**：reasoning 占 82–95%，`xhigh` 是它的总闸——但用户明确「我更倾向
  xhigh」，不动。

## 三之二、第二次采样：同一个病，更干净的样本（`1257d99c`）

写本片期间用户又发了一轮：新会话、**起点 ctx 只有 11.5k**（没有前一任务的历史），
被审提交 25 个文件 / +1959 / **diff 2556 行**。

| | 爬到 200k 用了 | 压缩点 | 轮数 | 墙钟 | 未缓存 |
|---|---|---|---|---|---|
| kloop | **10 轮** | 第 16 轮 | 90（**仍在跑**） | 55.3m+ | 1.53M |
| codex | **31 轮** | 靠后（峰值 244k 在轮 45） | 78（**已完成**） | 36.1m | 1.10M |

**两边都压缩了一次**——所以压缩本身不是区分点，**压缩发生在第几轮**才是：kloop 在
第 16 轮就压缩，之后还有 74 轮要在「失忆」状态下跑；codex 撑到接近尾声。

**这也推翻了本片第二节顺手提出的「跨任务历史累积」假设**：这次是全新会话，起点
11.5k，照样第 16 轮压缩。真正的变量是**每轮增量**。

差别在开局怎么取 diff：

```
kloop 轮3：把整个 diff 导出成文件
      轮4：read(gateway-1257d99c.diff, 450) × 6 并发
           ctx 20,574 → 53,965，其中未缓存 53,965（整轮零命中）

codex 轮2：wc -l <几份文档>                       ← 先探大小
      轮3：git diff … -- <file>.go <pkg>.go …   ← 按文件分批
      轮4：git diff … -- <file>.go …        ← 继续分批
```

不是「bash 对专用工具」的差别——**是一次全取对分批取**。kloop 读的本来就是 diff，
问题是 2556 行一次性进了上下文。

而压缩之后的形态，与第三节那个样本几乎逐位复现：

| | 压缩前 | 压缩后 |
|---|---|---|
| `f11e88ca` | 13 轮 / 3.6 工具每轮 / 单工具轮 46% | 45 轮 / **1.5** / **87%** |
| `1257d99c` | 15 轮 / 5.4 工具每轮 / 单工具轮 33% | 90 轮 / **1.4** / **86%** |

两个独立样本、不同提交、不同起点上下文，压缩后的批处理度都塌到 1.4–1.5、单工具轮
都在 86–87%。**这不是某一轮的偶然。**

### 顺带纠正一次方法错误

本轮我曾以「与 plan 98 冲突」为由排除「让模型多用 bash 组合命令」这个方向。用户
当场指出：**这要用实践说话，不是拿 plan 内容说话**。他是对的，而且这正是 plan 125
禁止的那种论证（拿仓库自己的文字当证据）。改用四轮实测重看，结论反过来：前 3 轮
`read_file` 行数 630 / 1250 / 1360 / 2280，对应轮数 5 / 12 / 17 / 60，单调，没有例外；
而最快那轮的开局恰恰是一条 bash 组合命令（`git diff --unified=100`）。plan 98 的射程
是「别用 bash 做 grep/cat/find/sed」——那几样有专用工具对应，而**「取改动及其上下文」
在 kloop 里没有任何专用工具，只存在于 bash**。

## 四、做了什么

### 4.1 摘要保住「已确认的事实」——降低压缩的代价

`COMPACT_INSTRUCTION` 补第 11 节，与第 7 节（sub-agent results）同构：

> 11. Established facts — what the earlier work already read and settled: the file
> and symbol, the behavior the code actually has, the value or branch that was
> confirmed. Carry the conclusion, not the intention to check it. Everything listed
> here is answered: the next agent uses it without opening the file again. Section 4
> records the actions taken; this one records what they proved.

第 7 节的注释里写着它存在的理由：*a summary that drops what a child reported makes the
parent redo the child's whole investigation（measured: 83 of 99 rounds）*。**自己读到
的事实是同一回事**，只是丢失时不表现为「重做子任务」，而表现为**批处理度塌掉**：
3.6 → 1.5 工具/轮。常量的 doc comment 里补了这条来历和这组数字。

两个断言片段进 `compaction_prompt_keeps_its_load_bearing_clauses`：`Established facts`
锁住这一节的存在，`without opening the file again` 锁住它的**用途**——只剩标题的话，
这一节会退化成又一份「做过什么」的清单，而那正是第 4 节已经有的。

plan 86 第 25 条要求 `COMPACT_SYSTEM` / `COMPACT_INSTRUCTION` 保持**稳定字节**，禁的是
掺入日期、cwd、session id、trigger、token estimate 这类每次都变的元数据；加一节固定
文本不违反它，改完之后仍然逐字稳定。

### 4.2 开局少拉一点——推迟压缩本身

SKILL.md 开头那段（`Read the whole diff first`）之后补两句，一句管小改动、一句管大
改动：

> Take that diff with enough context that it answers the next question on its
> own — tens of lines on each side, not the default three. Opening the whole file
> is the expensive way to get the same context back, and most of what it returns
> is unrelated to the change. When the change is large, take the diff a few files
> at a time rather than in one piece: everything already in context is carried
> into every round that follows, and the rounds after a summary are the expensive
> ones.

`Read the whole diff first` 说的是**覆盖全部改动**，不是**一次性搬进上下文**——两句
话不冲突，但原文没有把这层说破，于是 2556 行的 diff 被整个读了进来。

两条断言进 `skills.rs`：`not the default three` 锁住上下文宽度，
`at a time rather than in one piece` 锁住分批。两条都是**降本**条款——它们减少读取
量，不增加任何动作。

## 五、非目标

- **不加任何会引出额外动作的审查条款。** 本轮调查途中曾补过一条「改动可能让它没碰过
  的文档失效」（`6de3a0e2` 上 codex 报了一条 kloop 漏掉的文档漂移 P2，而 kloop 两轮前
  自己引用过那句话）。用户当场按住：**可以忍受漏掉文档这类无关紧要的，速度优先**。
  该条款已从 SKILL.md 撤回，不留半条。**判据留下：给审查加的规则，先问它是降本还是
  加本；加本的一律要用户点头。**
- **不碰跨任务的历史累积**——但理由不是它有害，而是**它被证伪了**：`1257d99c` 是全新
  会话、起点 11.5k，照样第 16 轮压缩。真正的变量是每轮增量，不是起点高低。
- **不改压缩的触发阈值、`chars/4` 估算或 `max_turn_growth`**（plan 86 的边界）。
- **不动 effort**。

## ✅ 已完成（2026-09-09；提交 SHA 以本条所在提交为准）

`crates/core/src/compact.rs`：`COMPACT_INSTRUCTION` 第 11 节，常量 doc comment 补第五
条来历，测试补两个断言片段。
`crates/core/src/skills/code-review/SKILL.md`：开头 diff 那段之后补两句（上下文宽度、
大改动分批），`crates/core/src/skills.rs` 补两个断言片段。
全部是 prompt / skill 文本改动，无行为变更，README 不涉及。

### 验证

- `compaction_prompt_keeps_its_load_bearing_clauses` 通过；`cargo test -p kloop-core
  --lib compact` 42 passed。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -D warnings`、
  `cargo test --workspace` 全绿。

### 下一轮复查

两个数字，分别对应 4.1 和 4.2：

1. **压缩之后的平均工具/轮**（4.1 是否奏效）。基线：两个样本压缩后都是 1.4–1.5，压缩前
   3.6 / 5.4。没有回到 3 以上，就该怀疑「摘要写了但模型不信任它」，而不是再往摘要里加节。
2. **爬到 200k 用了几轮**（4.2 是否奏效）。基线：kloop 10 轮，codex 31 轮。这个数字
   直接决定压缩发生在第几轮，而压缩之后的轮次是最贵的。

次要指标：压缩后的轮数（基线 45 / 74）与墙钟（`f11e88ca` 27.6 min、`1257d99c` 55min+）。
