# Plan 202 — 摘要一直失败,就别一直付钱

> 来源:2026-09-23 读 `refs/chord`(`keakon/chord@cce05db`,MIT)后,用户点名「1,2,3,4 都立 plan」,
> 这是第 2 条。出处见 `refs/README.md`「chord 固定源码调研(2026-09-23)」。

## 一、为什么

预测性压缩失败时,`compact_predictively`(`core/src/agent.rs:522`)只发一条
`predictive compaction failed: …` 的 note,然后照常采样(`agent.rs:568-572`,注释写明
"Predictive failure is not fatal")。这个取舍本身对:压缩失败不该杀掉这一轮。

问题在下一轮。**下一轮的估算仍然超线,于是又压一次**——`compact_predictively` 每轮都会进来,
而它没有任何"刚刚失败过"的记忆。provider 持续出错(网关抽风、摘要模型被限流、摘要请求本身
稳定地超时)时,**每一轮都白付一次完整的摘要请求**:那是把几乎整段历史发出去的调用,是一轮里
最贵的那一笔。

还有一个放大因素:`sample_summary`(`core/src/compact.rs:546`)**没有任何重试**——普通采样有
3 次指数退避(`agent/sampling.rs:83`),摘要请求一次 5xx 就是失败。于是一次瞬时抖动也会进入
上面那个循环。

chord 的做法(`internal/agent/compaction_failure_policy.go:14-16, 134-151`):**连续失败 2 次,
暂停自动压缩 3 个 turn**。

## 二、形状

### 2.1 断路器

会话级、只在内存里的一个计数器(不进 rollout;resume 从零开始):

- **预测性压缩失败**(`Err`,且不是因为用户取消)→ 连续失败数 +1。
- **连续失败达到 2 次** → 断开:接下来 **3 个 turn** 内跳过预测性压缩,并发一次 note:
  `automatic compaction paused for 3 turns after 2 consecutive failures; /compact still works`。
  断开期间不再每轮重复这条 note。
- **任何一次压缩成功**(预测性、被动、手动 `/compact` 都算)→ 计数清零、断路器闭合。
- 暂停期满 → 自动闭合,计数清零,下一次超线照常尝试。

**以 turn 计,不以 round 计。** 一个 turn 可能有几十轮;"暂停 3 轮"在一个长 turn 里一眨眼就过去,
起不到作用。

**不受断路器影响的两条路:**

- **被动压缩**(`recover_from_overflow`,`agent.rs:579`):provider 已经拒绝了请求,不压就只能
  失败,它本来也每 turn 只试一次(`overflow_compact_attempted`)。断开期间照样尝试;成功则闭合。
- **手动 `/compact`**:用户明确要求,永远执行。

### 2.2 摘要请求也要有瞬时重试

`sample_summary` 碰到**可重试**的 provider 错误(`ProviderFailure::is_retryable()`,与普通采样
同一个判据)时,按普通采样同样的退避重试,最多 3 次,尊重 `Retry-After`。**上下文超长不在此列**——
那条已经有自己的处理(`compact_once` 里缩小再试,`compact.rs:398-429`)。

有了这层,断路器的"连续 2 次"数的就是**真失败**(重试耗尽、或不可重试的错误),不是一次抖动。

**重试逻辑不要写第二份。** 开工时看 `sampling.rs` 里退避与 `Retry-After` 的那段(`sample_with_retry`
第 150-180 行附近)能否抽成一个小函数两处共用;抽不干净就只共用"算延迟"那一步。

## 三、断路器放在哪

**跟 `History` 同寿命**:它是会话级状态,而 `History` 已经是会话级内存状态的归处(usage anchor、
`observed_overflow_ceiling` 都在那儿,同样不进 rollout)。子 agent 有自己的 `History`,于是各自一个
断路器——子 agent 的摘要失败不该让父 agent 停压,反之亦然。

`/clear` 与 rebase 时重置;`replace_all` 本身就是"成功",闭合。

"turn 数"从哪来:开工时看 turn 的入口(`turn_rounds` 开头,或 `run_turn`)在哪里能拿到
`&mut History`,在那里推进一次"已过 turn 数"。不要从 rollout 的 `TurnTerminal` 反推。

## 四、开工时必须问用户的点(只有一个)

**阈值用 chord 的「连续 2 次、暂停 3 个 turn」,还是更保守一点?**

- **照抄 2 / 3(推荐)**:加上 2.2 的重试之后,连续两次真失败已经是很强的信号;暂停 3 个 turn
  期间还有被动压缩兜底,不会因为停压而把会话撑爆。
- 1 / 5 之类更激进的取值:更省钱,但一次偶发的不可重试错误(比如摘要模型偶尔调了工具被拒)
  就会停压好几个 turn。

## 五、测试

- mock provider 让摘要请求稳定失败:第 1、2 个 turn 各尝试一次预测性压缩,第 2 次失败后出现
  暂停 note;第 3~5 个 turn **不发**摘要请求(抓请求计数);第 6 个 turn 恢复尝试。
- 暂停期间超线导致 provider 拒绝 → 被动压缩照常发生;它成功 → 断路器闭合、计数清零。
- 暂停期间 `/compact` → 照常执行;成功后闭合。
- 一次失败后成功 → 计数清零(第二次失败不触发暂停)。
- 用户取消导致的失败**不计数**。
- 摘要请求一次 5xx 后成功 → 压缩成功、计数不增(2.2 的重试)。
- 摘要请求上下文超长 → 走原有缩小重试,不走 2.2 的退避(请求序列断言)。
- 父子 agent 断路器互不影响。
- 用例整对象断言 note 文案。

## 六、完成时要一起做的

- `rust/DESIGN.md` 压缩一节:先读现在怎么描述预测性失败的("not fatal"那段),改写成
  "不致命,但连续失败会暂停",并写明被动与手动不受影响。
- HANDOFF.md 若有新教训则记。
- **若 plan 200–204 中其余几条都已完成**(本条是最后一条):按 `refs/README.md` chord 一节退休本地 clone——
  那一行改为"已退休",确认 HEAD 仍是 `cce05db`、工作树干净后删除 `refs/chord`。

## 七、完成记录

✅ 2026-09-24,提交 `48d0d93`。开工问答:阈值照抄 2 / 3(用户「同意」)。

- **断路器**:`compact::CompactionBreaker`,挂在 `History` 上(`compaction_breaker()`),
  不进 rollout;`History::new`/`resume` 从闭合开始,`rebase` 重置,`replace_all` 即成功——
  预测性、被动、`/compact`、`/clear` 都经它,所以"任何成功都闭合"不需要在三个调用点各写一遍。
  turn 数在 `turn_rounds` 开头 `begin_turn()` 推进。触发的那个 turn 剩下的轮次也跳过
  (同一 turn 里再试也是同一个坏 provider),之后再暂停 3 个 turn;触发时计数清零,于是期满后
  要重新攒满 2 次才会再断。取消不计数(`compact_predictively` 本来就先判 `cancel`)。
- **摘要重试**:`sample_summary_with_retry`,判据 `is_retryable() && !is_context_overflow()`,
  最多 `MAX_ATTEMPTS` 次;退避抽成 `agent::sampling::retry_delay`,采样与摘要共用,
  `MAX_ATTEMPTS` 同一个常量。重试没有发 note(`compact_once` 不持有 `Ui`,为一条 note 改签名不值)。
- **测试**:`agent/tests.rs` 六个 turn 的请求计数与逐 turn note 整体断言(`[1,1,0,0,0,1]`)、
  暂停期被动压缩照常且闭合、取消不计数;`compact.rs` 断路器状态机两条、503 一次后成功、
  不可重试只发一次、transport 耗尽发 3 次同样的请求、超长走缩小而非退避(请求变短)、
  暂停期 `/compact` 照常且闭合;`subagent.rs` 子 agent 的 `History` 断路器从默认开始。
  另有两条旧用例因可重试失败被重试成功而改成不可重试失败(教训 189)。
- chord clone **未退休**:203、204 尚未完成。

