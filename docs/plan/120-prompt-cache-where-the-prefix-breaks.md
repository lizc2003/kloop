# Plan 120 — prompt cache：前缀到底在哪一字节断了

> 来源：与 Plan 119 同一批 dogfood 数据（`审查：7fed2427`，kloop 59 轮采样，
> 2026-09-04 14:33–15:10，gpt-5.6-sol / gateway / Responses）。
>
> Plan 118 第四节的结论是「缓存命中率是**结果**而不是病因，病因是每轮往上下文里
> 塞多少」。这次的数据给出一个该结论解释不了的反例，需要修正并往下查一层。

## 一、反例：相邻两轮只差 145 token，命中从 100% 掉到 0

`total = input_tokens + cache_read_input_tokens`（kloop 的 usage 把已缓存部分从
input 里剥出来记）：

```
time       total    delta  gap_s  cache_read   hit
14:34:10   44484   +33146    26s      44288   100%
14:34:16   44629     +145     6s          0     0%
14:34:21   44758     +129     5s          0     0%
14:34:33   44889     +131    12s          0     0%
14:34:54   77067   +32178    22s      44544    58%
```

这三轮是三次连续的 `task_create`，每轮只往上下文加了一个 tool_call 加一条结果，
**prompt 增量 129–145 token，间隔 5–12 秒，命中率从 100% 直接掉到 0**，
然后 22 秒后那个 44k 前缀又命中了。

- 不是增量大：145 token。
- 不是 TTL：6 秒。
- 不是前缀不存在：它前一轮命中过 44288，后一轮又命中 44544。

**「每轮塞太多」解释不了 145 token 的那一跳。** Plan 118 第四节关于布局的证伪仍然
成立（`instructions` 与 `developer input` 两种形状都能进缓存），但把命中率整体归给
上下文增量这一句要收回一半：增量决定了 miss 的**代价**，决定不了 miss 的**发生**。

全会话：总 prompt **8.30M**，cache_read **1.89M**，命中率 **22.8%**。同日同 provider
的 codex 会话：input 13.23M，cached 8.21M，**62.1%**。claude（Anthropic 侧）主 agent
第一轮 33 次采样，完全未缓存的 input 合计 **66 token**，cache_write 159k。

另一个形状特征：会话后半段的 cache_read 长期钉在 **6656 / 10752 / 14848**——都是 128
的整数倍，而 total 已经 200k+。也就是说大部分时间只有最前面十几 k 命中；但 15:01:14
命中过 112128、15:03:57 命中过 141824、15:06:24 命中过 127488，说明大前缀**能**命中，
只是不稳定。

## 二、三个待查假设

1. **kloop 的请求序列化在轮次之间不稳定。** 最可疑的三处：并行工具结果如何合并成
   消息（那次一轮最多 8 个并行调用）、Responses 的 reasoning item 回传形状
   （`store: false` 下每轮要把上一轮的 reasoning 原样送回）、offload 指针文案。
   任何一处在同一段历史上序列化出不同字节，前缀就在那里断。

   **参考项目给了这条假设一份现成的检查清单**：codex 的
   `codex-rs/core/src/client.rs:305` 把 `ResponsesApiRequest` 逐字段解构来判断请求
   是否等价，注释明写「Keep the destructuring exhaustive so new request fields
   require an explicit reuse decision」——`client_metadata` 和 `stream_options`
   被显式排除在外；同一文件的 `response_items_equal_ignoring_internal_metadata` /
   `clear_internal_chat_message_metadata_passthrough` 更是专门为「回传历史 item
   之前先抹掉内部元数据」而存在。也就是说 codex 明确知道 item 里会混进不该参与
   前缀比较的字段，并且有一处地方管这件事。**kloop 没有任何等价物**：先照这条
   线查 kloop 的 history → request 投影里有没有逐轮变化的字段跟着进了 input。
2. **provider 侧缓存写入是概率性的。** Plan 118 的实验 1 里，布局 A 同一请求连发
   三次，前两次都 miss，第三次才 98.9%——当时当噪声处理，配合这次的数据看，更像是
   写入本身不保证发生。
3. **大 prompt 的写入被截断。** 命中档位长期停在十几 k，暗示只有较小的前缀被真正
   写入过；但 141k 的命中又推翻了简单的"上限"说法。

## 三、方法：把请求体落盘，逐字节 diff

现在没法判定，是因为**没人看过 kloop 实际发出去的字节**。前两个 plan 的结论都是从
usage 数字反推的。

**改动。** 加 `KLOOP_DUMP_REQUEST=<dir>`（默认关闭，只认显式路径）：每次 provider
采样把即将发出的请求体原样写成 `<dir>/<session>-<ordinal>.json`。落的是 body
（instructions / input / tools），**不含 header，因此不含 API key**；body 含用户
代码，dump 目录不得提交，plan 里写明、`.gitignore` 不管这事。

拿到 dump 之后：

- 对连续两轮做逐字节 diff，取**第一个差异的偏移量**。
- 差异出现在"上一轮 body 末尾"之后 → 前缀稳定，问题在 provider 侧：把两轮 body 的
  公共前缀长度、命中量、时间戳整理成一份可以直接交给 gateway 的证据。
- 差异出现在前缀内部 → 是假设 1，按第一个差异点直接修。

跑的会话要覆盖那次的形状：一轮 8 个并行 `read_file`、一轮单 `bash`、连续三轮
`task_create`（就是上面那三轮的最小复现），至少一次 reasoning 回传。

## 四、验收

分支一（前缀不稳）：找到断点并修复后，同形状的复现会话命中率从 22.8% 升到 60% 以上，
且上面那三轮 `task_create` 不再 0 命中。

分支二（provider 侧）：交出逐字节证据——公共前缀 N 字节、命中 M token、M ≪ N——
结论写进 HANDOFF，**不再用 usage 数字反推第三次**。

无论哪个分支，`KLOOP_DUMP_REQUEST` 留下来：它是这类问题唯一的一手证据来源。

## 不做的

- 不改 `COMPACT_SYSTEM` / `COMPACT_INSTRUCTION` 的字节（Plan 86 的稳定摘要请求约束；
  Plan 119 加的第 10 节是那次的一次性变更，之后同样固定）。
- 不为了提高命中率去缩减每轮上下文——那是 Plan 118 第一节已经在做的事，与本片正交。
- 不引入 provider server-side compaction 或 `previous_response_id` 有状态模式。

## 验证

- `cargo fmt` + `cargo clippy --all-targets -D warnings` + `cargo test --workspace` 全绿。
- dump：环境变量未设时零开销、不创建目录；设了之后每次采样一个文件，内容与实际发出
  的 body 逐字节一致（用 mock provider 断言）；body 里不出现任何 header 字段。
- 真实链路的实验记录（命中率、断点偏移）写进本文件末尾，不入代码。
