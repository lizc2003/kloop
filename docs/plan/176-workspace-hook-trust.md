# Plan 176 — 工作区里的那条 hook,凭什么能跑

> 来源:2026-09-21 对 `zai-org/ZCode@872ad960de7ec172591f7e1952f7849229f94521`
> (Apache-2.0,公开仓库)的一次调研。那个库退休时,值得留下的设计里最实的就是这一条。
> **这是一个条件 plan:触发条件未到之前不要开工**,见第五节。

## 一、kloop 今天没有这个问题,因为口子还没开

`RuntimeSettings::load` 里 hooks 只有一个来源:`load_hooks(table)`,`table` 来自
`UserConfig`,也就是 `~/.kloop/config.toml` 这一个文件(`rust/crates/cli/src/startup.rs`)。
工作区里的任何文件都不能让 kloop 执行命令。

**这是当前的安全姿态,不是疏漏**——写下来是为了防止以后有人把它当成"忘了做"而顺手补上。
口子一旦开(团队共享的 pre_tool 检查、仓库自带的 lint gate、CI 与本地同一套钩子),
等式就变成 **`git clone` 一个仓库 = 可能执行它带的命令**,那时才需要下面这套东西。

## 二、ZCode 的模型:六件值得照搬的事

源码在 `apps/zcode-cli/packages/contracts/src/hooks/workspace-hook-trust.ts`(契约)与
`core/src/hooks/workspace-hook-trust-{evaluation,records,coordinator}.ts`(判定)。

**1. 两级 digest。** `bundleDigest` 是工作区发现到的整个 hook 集合的 sha256,
`hookDeclarationDigest` 是单条声明的 sha256。改一条只让那条回到待审,不推翻整包;
反过来,授权记录里同时留下 `bundleDigestAtGrant`,所以"当时批的是哪一版"可回溯。
canonical 化的字段是 (event, matcherIndex, hookIndex, sourceFileIndex, sourceRelativePath,
matcher, command, resolvedTimeoutMs, resolvedMaxOutputBytes, type 及其专有字段)——
**解析后的值而不是原文**,所以改个缩进或键序不会让已授权的 hook 回到待审。

**2. 七个状态,一张表定准入。** 状态是 not_applicable / pending_trust / trusted_persistent /
blocked_untrusted / blocked_policy / revoked / stale_digest;`STATE_ADMISSION_MAP` 把每个状态
钉到 (admissionClass, effectiveRunnable, reasonCode) 三元组,而且 schema 会校验任何
effective-state 对象与这张表一致。**准入结论不许调用点自己拼**——和 kloop plan 63
冻结 `EffectiveWorkspace` 是同一种做法。

注意归类:`stale_digest` 和 `revoked` 都归 `pending` 而不是 `blocked`。改过的 hook 是
"重新问一次",不是"封杀"。只有 untrusted 与 policy 两种才是 blocked。

**3. 槽位 vs 摘要——`stale_digest` 是怎么判出来的。** 授权记录里除了 digest,还留了当时的
**槽位**:(eventAtGrant, sourcePathAtGrant, sourceDiscoveryOrderAtGrant, matcherAtGrant,
matcherIndexAtGrant, hookIndexAtGrant)。发现一条新 hook 时,若某条旧记录的槽位与它完全相同
但 digest 不同,就判 `stale_digest`("你信任过这个位置上的那条,它的内容变了"),
而不是普通的 `pending_trust`("这是一条没见过的新 hook")。两者给用户看的提示不该一样。

**4. policy 三模式带修订号。** `deny` / `user_decides` / `allow_trusted_only`,每个都带
`policyRevision`(默认 `builtin:user-decides:v1`)。修订号的用处是:策略换了之后,
旧的授权记录能被判为"在旧策略下批的"。

**5. 评估顺序本身就是安全边界。** 优先级是:policy=deny → 信任库损坏 → allow_trusted_only →
有持久授权 → 被撤销 → 槽位陈旧 → 待审。**第二条是重点:信任库读不出来时判
`blocked_untrusted`,而不是当作空库然后一路问用户**。fail closed 写在判定顺序里,
不靠调用点记得。最后 `effectiveRunnable` 还要再乘一次 `configuredEnabled && policy !== deny`,
即使映射表已经说了 configured。

**6. reason code 要能区分"读不出来"和"写不进去"。** 它的 reason code 全集有 20 个,
其中 `config_unreadable` 与 `config_write_failed` 分开,源码注释写明这是修过的 bug:
读失败曾被报成写失败,让用户去重试一个根本没发生的写入。

## 三、落到 kloop 会是什么形状

已经有的地基比想象中多:

- **`WorkspaceIdentity`**(plan 63)就是 trust key 的前半截,不用另造"工作区身份"。
  key = (WorkspaceId, declaration digest)。
- **`ProjectStore`**(plan 63:用户私有、按 workspace 分区、global 只留 deny/ask)
  正是信任记录该待的地方——它已经承担了"这个工作区被批准过什么"的语义。
- **`HookDef` 比 ZCode 的简单**:command 是 argv 且不过 shell,没有 `shell` / `async` 字段,
  canonical 形式因此更短(event, matcher, command argv, timeout_ms + 来源位置)。
- **policy 三模式与 kloop 的 deny/ask/allow 同构**,不必引入第二套词汇。

要新造的只有三样:工作区 hook 的**发现**(读哪些文件、顺序、能不能被 symlink 骗——
照 CodeWhale 那条 `O_NOFOLLOW` 结论)、**digest 与信任记录**、**审批入口**。

## 四、不抄的

- **zod schema + superRefine 那一层**。Rust 里"状态与准入三元组一致"用类型和构造函数就够,
  不需要运行期再校验一遍自己刚构造的对象。
- **`interactionId` / `generation` / `superseded` 的多客户端评审流**(315 + 607 行)。
  ZCode 要同时服务 TUI、Web、Desktop 和远程,kloop 的 TUI 是单客户端。评审竞态的
  真问题(审到一半配置又变了)用"批准时比对 bundleDigest,不一致就重问"就能挡住,
  不必一上来就铺 flow id 与 10 分钟 deadline。
- **hook 专属 telemetry**。

## 五、触发条件与开工时要定的点

**触发条件:真的要开项目级 hooks 的时候。** 在那之前"只信 `~/.kloop/config.toml`"
是更强的答案,这个 plan 不该开工。

开工时先问清:

1. 工作区 hook 声明放在哪个文件、发现顺序是什么(kloop 目前没有项目级配置文件这个概念)。
2. digest 算在解析后的 canonical 结构上(ZCode 的做法,推荐)还是文件原文。
3. 信任记录进 `ProjectStore` 的哪一层,与现有 project-scoped allow 是同一份文件还是分开。
4. 审批走现有权限审批通道,还是会话启动时的一次性评审。
5. 全局配置里的 hooks 是否永远 `not_applicable`(推荐是:那是用户自己写的,不该被问)。
