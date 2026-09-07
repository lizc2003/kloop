# Plan 124 — effort 要上时间线；一个 flaky 的 PTY 测试

> 来源：2026-09-07 dogfood 收尾。两件小而确定的事，都从 Plan 122/123 的调查里
> 掉出来。用户拍板「3 都做，并且不用考虑兼容性」。

## 一、rollout 记不下 effort

Plan 122 第一节我把 effort 误标成"三方对照里唯一未对齐的变量"，被用户当场纠正
（两边都是 `xhigh`）。根因是 **kloop 的 rollout 里查不到它**：
`ProviderRouteReceipt` 记了 provider / model / apiFamily / endpointFingerprint，
唯独没有 effort。

而 effort 不是无关参数——**Plan 123 那个死角的触发率完全由它决定**（xhigh 把
「已有 thinking、还没有可回放 block」的窗口拉到几乎整个请求）。以后每一次
「为什么这个会话行为不同」的排查都会撞上这个空白。

### 只记初始值会留下一个会说谎的字段

`/effort` 是会话中途可改的，而且**不 bump revision**：`set_effort` 只动内存状态，
`append_provider_route_changed` 要求 `revision == previous + 1`，所以 effort 变更
在 rollout 里完全没有痕迹。只把 effort 加进 `provider_route_initial` 的话，一个
中途改过 effort 的会话，transcript 会带着十足的信心显示错误的值——**比不记更糟**。

所以两半都要做：

1. `ProviderRouteReceipt` 加 `effort: Option<ReasoningEffort>`（`None` 表示不发
   effort 字段，与任何具名档位都不同，是一个真实状态而不是缺省）。
2. `/effort` 改值时**作为一次 route revision 落盘**——它确实换掉了
   `FrozenProviderRoute`（effort 是它的一部分），而 `validate_timeline` 只要求
   revision 递增 + boundary 递增 + source 为 `ExplicitSwitch`，不要求
   provider/model 必须变化。continuity 原样带过：同 provider 同 model，reasoning
   回放的条件一点没变。

### 实施时撞到的边界

**第一个 turn 之前打 `/effort` 是常态**（启动后先设档位），而那时 route 时间线
还是空的，`append_provider_route_changed` 会因「timeline is missing」失败。

第一版在 effort 命令里直接调 `ensure_initial_provider_route` 兜底，**第二次
`/effort` 就挂了**：`ensure` 在时间线非空时会校验最后一条 receipt 与传入的
`cfg.provider_route` **revision 相同**，而 cfg 要等前端重新冻结才跟上，中间那一
拍必然不等。改为先问 `history.has_provider_route()`，只在真空时开线——不依赖
「前端记得重新冻结」这个时序。

## 二、一个 flaky 的 PTY 测试

`tui_pty::two_turn_overflow_commits_without_scroll_regions_then_repaints`：同一个
二进制连跑两次，一次红一次绿；单独跑三次全绿。失败在
`assert_eq!(final_frame.count("Type a message"), 1)`，实测 0。

**不是产品 bug，是断言时机。** `wait_for` 返回第一个满足 predicate 的快照，而
predicate 是「出现 SECOND_TAIL 且 Working 消失」——**turn 的最后一段输出和重画
后的 composer 落在不同帧里**，于是它返回的是一个过渡帧：Working 已经走了，输入
框还没回来。整体跑时机器负载高、时序更松，正好命中那个窗口。

同一个窗口还有第二个后果：第一处 `wait_for` 返回后立即 `write(b"second turn")`，
composer 若尚未就绪，这次输入会落空。

改法是两处 predicate 都等到 composer 回来。**这不会掩盖真实缺陷**：composer 真
不回来的话，`wait_for` 会超时 bail，依然是红的。

> 为什么值得单独修：一个每次全量都可能红一次的测试，会让「这次红了要不要管」
> 变成常规判断，而那正是真回归溜过去的方式。

## 非目标

- 不改 `ReasoningEffort` 的取值、不校验哪个模型接受哪个档位（那是模型的合同，
  `set_effort` 的注释已写明）。
- 不动 provider switch 自身的 revision/continuity 语义。
- 不为旧 rollout 做迁移或双读（用户明确：不用考虑兼容性）。`effort` 是
  `Option` + `skip_serializing_if`，旧文件读成 `None` 是这个字段的正确建模，
  不是为兼容写的代码。

## 验证

- `cargo fmt` + `clippy -D warnings` + 21 个测试二进制逐个跑。
- `effort_changes_land_on_the_route_timeline`：turn 之前设 effort 会自己开时间线；
  连续改动逐条落盘；重复设同一个值不产生 revision；`unset` 记为 `None`。
- flaky：修前同一二进制一红一绿，修后**连跑 10 次整体全绿**。
