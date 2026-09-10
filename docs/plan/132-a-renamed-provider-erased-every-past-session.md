# Plan 132 — 改一个 provider 名字,过去的会话就全不认识了

> 来源:2026-09-10,用户 dogfood:`kloop -c` 打印
> `[resumed session 20260910-141535: 20 message(s)]` 之后立刻
> `Error: unknown provider 'gw_cn'`。原话:「换 provider,以前的 session 加载不了了」。
>
> 拍板:**自动落到当前默认 provider;不考虑兼容性。**开工中用户又补了一句定语义的话:
> **「session 里存储的 provider,只需要做参考就行了吧」**——解析得了就接着用它,解析
> 不了就换成当前默认,而不是"解析不了就拒绝开会话"。

## 一、根因:把「配置变了」当成了「时间线坏了」

Plan 92 给 route timeline 定的是 fail-closed:「restore 只接受完整的新 route timeline
……revision 缺失、重复、倒退、未知 source、provenance 引用不存在的 revision 时 fail
closed」。那条规矩针对的是**转录损坏**——它描述的事情自相矛盾,继续跑就是在编造。

但 `restore_route()` 走的是另一件事:拿**今天的 catalog** 去校验**昨天的 receipt**。
用户在 `~/.kloop/config.toml` 里把 `gw_cn`/`gw_router` 换成 `polo`,是一次完全正常的
配置修改,转录一个字节都没坏——坏的只是"那个名字今天还在不在配置里"。两件事撞进了同
一个 `Err`,于是正常操作被当成损坏处理。

波及面比报错那一行大得多。`~/.kloop/projects/v1` 下 114 个会话的 receipt 统计:

| providerId | receipt 数 |
| --- | --- |
| `gw_router` | 7186 |
| `gw_cn` | 34 |
| `polo` | 1 |

即**除了换完之后新开的那一个,其余 113 个全部打不开**,不只是报错点名的 gw_cn。

而且不止改名会中招。`validate_receipt()` 还比对 endpoint fingerprint、fallback model,
以及"旧的 primary_model 是否仍在今天的 models 允许列表里"。所以 id 不动、只改一个
base_url,或者从 models 里删掉一个不再用的旧模型,历史会话同样全灭。**一个只增不减的
配置文件才不会踩到——而配置文件本来就是用来改的。**

## 二、第二处:历史 receipt 也在被今天的 catalog 审一遍

`SessionProviderState::from_timeline()`(server 与 TUI 走这条)不止校验最后一条,而是
`for receipt in timeline { catalog.validate_receipt(receipt)? }` —— **整条历史**逐条审。
这意味着即使按第一节修好了"接着往下跑用哪条 route",一个中途 `/provider` 切过一次的会
话,只要那个旧 provider 后来从配置里消失,仍然永远打不开。

历史 receipt 记的是**当时发生了什么**,不是**现在还能不能用**。拿今天的目录去判定昨天
的事实合不合法,这个方向本身就是错的。

## 三、第三处(同一处缝里顺手捞出来的):server resume 只认 revision 1

`spawn_thread()` 在 factory 之后无条件调 `ensure_initial_provider_route(&cfg.provider_route)`,
而 resume 路径的 cfg route 是 `catalog.initial_route(...)` 造的——**revision 恒为 1**。
于是一个中途切过 provider(revision ≥ 2)的会话走 `thread/resume`,会撞上
`history provider route does not match the frozen operation route`。resume 采用会话最后
一条 route 的正确做法本来就该是 restore 而不是重造 initial,这次一并改掉。

## 四、做了什么

1. **新 receipt source:`recovered`**。`ProviderRouteSource` 加第三个变体,与
   `explicit_switch` 并列写在 `provider_route_changed` 行上。转录里这一跳是"配置里没有
   那个 provider 了,被迫改道",不是用户手敲的 `/provider`——两者在一个字段里区分开,
   几个月后读转录的人才不会把它读成一次不存在的用户操作。
2. **`History::adopt_provider_route()`** 成为 resume/fork/rewind 采用会话 route 的唯一
   入口:先按老规矩 `restore_route(最后一条)`;成功则原样接上(revision 不变,零副作用)。
   失败(unknown provider / route drift / unknown model / unavailable)则落到调用方给的
   当前默认 route,revision + 1,追加一条 `recovered` receipt,continuity 用与
   `switch_provider` 完全相同的投影法算(旧推理放不进新 rail 就是 `filtered`),并把一
   条 `RouteRecovery` 交回给前端去说人话。
3. **历史 receipt 不再拿今天的 catalog 校验**。`from_timeline` 只保留结构校验
   (`validate_timeline`)加"最后一条必须能 resolve";逐条 `validate_receipt` 删除。
4. **`sanctioned_switch` 认 `recovered`**。旧 rail 的 reasoning 之所以能从 request view
   里剥掉,靠的是"后面有一条 durable 的显式改道"——recovered 就是这样一条,否则恢复完
   第一轮就会撞 `reasoning mismatch is not authorized by a durable explicit provider switch`。
5. **server resume 改走 adopt**:`spawn_thread` 按 `has_provider_route()` 分岔——新
   thread 走 `ensure_initial_provider_route`,resume/fork 走 `adopt_provider_route`,
   第三节的洞随之补上。`resume_options` **仍然**把记录里的 provider/model 传给
   `ConfigFactory`——那就是用户说的"参考":真 factory 拿它从配置 catalog 里选路由,测试
   factory 拿它建自己的 catalog。变的是**建不出来不再是失败**:resume 时 factory 报错就
   去掉这个参考重建一次(默认路由),再由 `adopt_provider_route` 决定最终落点。
   `thread/start` 显式点名的 provider 继续硬失败——那是客户端自己从 catalog 里挑的,
   拼错了就该报错,不能悄悄换一家去计费。

## 五、非目标

- **不放宽 fail-closed 的其余部分**:revision 倒退/重复、provenance 引用不存在的
  revision、origin boundary 不在区间内、usage 身份对不上——全部照旧拒绝。这次只把
  "今天的 catalog 里没有这个名字"从**损坏**降级为**改道**。
- **不迁移旧文件**:`recovered` 是新写入的形状,历史文件一行不改。
- **不做"记住上次用的 provider"之类的选择逻辑**:落点就是当前配置的默认 route,一个。

## ✅ 已完成(2026-09-11;提交 SHA 以本条所在提交为准)

- `crates/protocol`:`ProviderRouteSource` 新增 `Recovered`(wire 上是 `"recovered"`)。
  route receipt 带 `#[serde(skip_serializing)]`,不出进程,所以这是纯内部 + rollout 形状。
- `crates/core/src/provider_route.rs`:`validate_timeline` 在 revision > 1 处接受
  `ExplicitSwitch | Recovered`;`from_timeline` 删掉逐条 `validate_receipt`(只留结构校验
  与"最后一条必须 resolve");新增 `RouteRecovery`(from/to + reason + continuity,
  `Display` 就是给用户看的那句话)与 `FrozenProviderRoute::with_reasoning_continuity`。
- `crates/core/src/history.rs`:新增 `adopt_provider_route(catalog, fallback)`——resume/
  fork/rewind 采用会话 route 的唯一入口;`switch_provider` 里的投影逻辑抽成
  `projected_continuity(next, source)` 与它共用;`sanctioned_switch` 认 `Recovered`;
  `append_provider_route_changed` 多收一个 `source`。
- `crates/core/src/rollout.rs`:`append_provider_route_changed` 收 `source`(拒绝
  `Initial`);`validate_provider_routes` 对 `provider_route_changed` 行只要求"不是
  initial",具体哪种改道交给 `validate_timeline`。
- `crates/cli/src/main.rs`:`restore_route(最后一条)` → `adopt_provider_route`,恢复时
  按既有 warning 样式打一行 dim note(**stderr**,不污染 `--headless --json` 的 stdout)。
- `crates/tui/src/lib.rs`:rewind(`WorkerMsg::Fork`)同样走 adopt,fallback 用会话当前
  正在跑的 route;顺序改成先 `rebase` 后 adopt(adopt 要往新分支的 rollout 写)。
- `crates/server/src/lib.rs`:`spawn_thread` 按 `has_provider_route()` 分岔(新 thread
  `ensure_initial_provider_route`,resume/fork `adopt_provider_route` 并把恢复发成
  `note`);resume 时 factory 建不出来就去掉 provider/model 参考重建一次;`ConfigFactory`
  文档写明"resumed thread 可能被调用两次"。

### 测试

- `history::tests::a_resumed_session_whose_provider_left_the_config_lands_on_the_default`:
  真写一个 rollout 文件,provider `gone` 上跑一轮带 reasoning 的对话,换成只有 `kept` 的
  catalog 后 adopt——整对象断言 `RouteRecovery`、revision 2、canonical 历史不变、request
  view 里 reasoning 被剥掉、**重读文件**拿到 `source: recovered` / `continuity: filtered`,
  以及**再 adopt 一次是 no-op**(resume 两次不能叠 revision)。
- `provider_route::tests::from_timeline_judges_only_the_route_the_session_continues_on`:
  历史那条指向已消失的 provider 照样恢复;最后一条指向已消失的 provider 仍然
  `UnknownProvider`。
- `server.rs::resume_readopts_the_recorded_route_and_recovers_when_it_is_gone`:
  一个 catalog 随重启变化的 factory——切到 `b` 后重启 resume 回到 `b`/revision 2(修掉
  第三节),再把 `b` 改名成 `c` 后 resume 落到 `c`/revision 3 并发出 note。

`cargo fmt` 干净;`clippy --workspace --all-targets -D warnings` 退出码 0;
`cargo test --workspace` 退出码 0(2 个真实凭据测试照例 ignored)。

**真实会话演练**(不动用户的项目分区):把用户那条 20 条消息的 `20260910-141535.jsonl`
(首行 `provider_route_initial` 写着 `gw_cn`)复制进一个临时 git 仓库的分区,再用一个
只声明 `polo` 的临时 HOME 配置跑 `kloop -r … --plain`:

```
[resumed session 20260101-000000: 20 message(s)]
[session was written on gw_cn/deepseek-v4-flash-0731, which no longer resolves
 (unknown provider 'gw_cn'); continuing on polo/gpt-5.6-sol — earlier reasoning
 is dropped from the request]
{'type': 'provider_route_changed', 'revision': 2, 'source': 'recovered',
 'providerId': 'polo', 'primaryModel': 'gpt-5.6-sol', 'continuity': 'filtered'}
```

第二次 resume 静默通过,文件里仍然只有 `initial` + `recovered` 两条 route 行。演练用的
临时分区与临时 HOME 已删除。
