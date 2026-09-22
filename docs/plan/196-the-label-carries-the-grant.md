# Plan 196 — 标签与授权合成一个文件

> 来源:2026-09-22。用户在 plan 193 落地后回看 `~/.kloop/projects/v1/<id>/`,问
> 「`trust.json` 与 `project.json` 合并,是不是更好」,随后自己把两个不满说清:
> **两个文件有重合**,以及 **`trust.lock` 只在初创写一次、不值得上锁**。
> 我第一轮反对合并(理由是"两个 crate 各写各的,合并后首启必丢 `granted_at`"),
> 用户点出 `trust.lock` 之后发现**那条反对建立在一个可以去掉的前提上**——见下。

## 一、裁决

**`trust.json` 并入 `project.json`,删掉 `trust.lock`。** 判据不变:分区目录存在 = 已信任。

撑起这次合并的机制变更只有一条:**`project.json` 从"每次启动比对后重写"改成"建一次,此后只读"**。

`session_store.rs` 原来的 `write_project_meta` 是"读回来比一遍字符串、不一样才写"。那是**防御性的,不是承重的**:
`ProjectId` 就是拿 anchor 当哈希输入算出来的(`project_id = hash("p1_", domain, anchor)`),
**id 定了 anchor 就定了、永不改变**。改成"建一次"之后,没有覆盖、没有读改写,也就没有锁可上——
我第一轮说的"grant 后紧接着 `ensure()` 会把 `granted_at` 抹掉"因此不再成立。

`granted_at` 成为**可选字段**:有 = 有人明确答过 yes;没有 = 有人先 headless/serve 跑过,
没人被问过。这正好是实话,而不是"缺字段"。

文件形状(键按 `json!` 的字母序;`granted_at` 缺席时不出现):

```json
{ "anchor": "/abs/path", "granted_at": "2026-09-22T07:46:15Z", "project_id": "p1_…", "version": 1 }
```

## 二、为什么锁本来就不该有

`permissions.json` 的锁护的是真 RMW:读回来、去重、`revision + 1`、写回去——没锁就会丢更新。
`trust.json` 的锁护的是**一次写入、且没有任何代码读回来的文件**;两个并发的 yes 写出的字节除了
秒级时间戳完全一样,`write_atomic` 本身也不会写出撕裂文件。私有权限校验在
`PrivateDir::open/ensure/write_atomic` 里(0600/0700、不跟随符号链接),与那把锁无关——
所以删锁不丢任何保证。**判据:上锁的前提是"有人会读回来再写回去";只写一次的文件,锁是仪式。**

## 三、改动

**core `session_store.rs`(定义形状)**
- `PROJECT_META` → 公开的 `pub const PROJECT_LABEL: &str = "project.json"`(cli 要拿它当私有目录里的文件名)。
- 新增 `pub fn project_label_bytes(project_id, anchor, granted_at: Option<&str>) -> Vec<u8>`:
  这个文件的形状与编码**只此一处定义**,cli 复用它而不是自己拼 JSON。
- `write_project_meta` → `write_project_label`,新规则:读得出来、且 `project_id` 与本次相同 →
  一个字都不写;缺席、读不出、或写着别人的 id → 写(自愈与身份守卫都保住)。

**cli `project_store.rs`(授权写这一份)**
- 删 `TRUST_FILE` / `TRUST_LOCK_FILE` / `TrustFile`。
- `grant_trust_blocking(&self, project_id, anchor: &Path)`:仍走 `ensure_project` 建目录,然后用
  `project_label_bytes(project_id, anchor, Some(now))` 经 `PrivateDir::write_atomic` 写 `project.json`,**不开锁**。
- `trusted_blocking` 不动(仍是 `is_dir`)。

**cli `trust.rs`**:`grant_trust_blocking(project_id, identity.partition_anchor())`。

**pty fixture**:手写 `project.json`(带真 anchor)代替 `trust.json`,保留 plan 193 那条
"手写、不经 API"的读路径交叉验证。

**文档**:DESIGN.md 的目录清单与 "Workspace trust" 段重写;HANDOFF 记教训。

## 四、不在这一版里的

- **不写迁移**。用户 2026-09-22 明确"不用考虑兼容性":老装机的 `trust.json` 成孤儿(没人读它),
  老项目丢 `granted_at`,仅此。
- **不加 create-new 原语**。grant 只在目录不存在时被调用,替换语义在这里没有可替换的东西;
  并发两个 yes 最多让后写者的时间戳胜,而没人读它。
- **不动 `p1_` 前缀**。用户 2026-09-22 另问"目录带个 p1_ 没有意义啊":它是 id 的**格式版本号**
  (`hash_id("p1_", …)` 拼上去、`validate_id` 拿它当门槛、provenance 里 `w1_…` 靠它可读),
  与路径里 `projects/v1/` 的"布局版本"是两个轴,将来换 id 算法可以独立走到 `p2_`。
  真要动只能**整个去掉**(`validate_id` 只认 64 位 hex),不能只从目录名剥掉——那会造出同一个身份的
  第二种拼法,而仓库刚用一次提交消灭这种双拼法。本次不动。
- 不改 plan 193 的历史文本(那是当时的裁决;今天的形状由 DESIGN.md 承载)。

## 五、✅ 完成

2026-09-22 当次会话做完,一次提交(SHA 以本条所在提交为准)。`make check` 全绿。

| 测试 | 锁住什么 |
|---|---|
| `project_store::tests::the_project_directory_is_the_answer_and_the_label_carries_the_grant` | 授权前不创建任何状态;授权后 `project.json` 整对象断言(含 anchor 与 granted_at);`permissions.json` 不受影响;**分区里只有这一个文件、没有 `.lock`**;重复授权仍是同一个答案 |
| `project_store::tests::the_grant_writes_the_anchor_a_cross_project_listing_reads_back` | 真实接线与两种交错顺序:core 先建标签、grant 再写进去(0644 会红),以及 grant 之后那次 `ensure()` 不抹掉 `granted_at`;anchor 经 `buckets()` 读回来等于 `partition_anchor()`(传 cwd 会红) |
| `session_store::tests::a_label_that_already_names_the_project_is_left_alone` | 合并的承重点:带 `granted_at` 的标签被随后的 `ensure()` 一字不改地留下 |
| `session_store::tests::a_foreign_or_unreadable_label_is_repaired` | 自愈仍在:别人的 id / **别的 anchor** / 不是 JSON / 空文件都会被重写成自己的标签 |
| `session_store::tests::created_state_is_owner_only` | 标签是 `0600`——不是整洁问题,CLI 的 grant 经同一个读路径打开它,0644 会让 grant 直接失败 |

真二进制验过(见提交信息):临时 `$HOME` 下交互式起一次、答 yes,`projects/v1/<id>/` 里只有
`project.json`(0600)且含 `granted_at`;第二次启动不再问,且该文件的 `granted_at` 与首次逐字相同。

## 六、复审后的修正(第二次提交)

用户复审提了四条,逐条核过**全部成立**,一个 `fix(plan196)` 提交:

1. **同一个文件两套模式,而其中一个写者拒绝非 0600。** core 用 `std::fs::write` 创建标签(0644),
   CLI 的 grant 经 `PrivateDir::write_atomic` 写入,而那个写路径第一步就读回来做
   `validate_private_permissions`——`mode & 0o077 != 0` 直接 bail。触发窗口正是"人盯着提示"的那段:
   并发的 `--headless`/`--serve` 建好 0644 标签后,答 yes 会失败。改:`OpenOptions::mode(0o600)`。
   这条用"先把测试弄红一次"验过——把 `0o600` 改回 `0o644`,新测试红在
   `project label contains credentials; restrict it to mode 0600`。
2. **anchor 失去了自愈。** 新规则只比 `project_id`,而 anchor 恰恰是 `buckets()` 唯一读的字段;
   旧代码比整段 body,写错下次会被修回。改:跳过条件是 `project_id` **且** `anchor` 都相同
   (`granted_at` 仍不在比较项里,承重点不受影响)。
3. **DESIGN.md 自相矛盾。** 我写的"a single write of a file nobody reads back"与上一段的
   `read_project_anchor` 冲突——准确说法是"没有任何读点拿 `granted_at` 做判断"。两处都改了。
4. 标签改回 pretty:旧 `trust.json` 就是 `to_vec_pretty`,而这是分区里唯一给人看的文件。

顺带:grant 的 anchor 接线补了测试(单测原本只喂编造的 `/work/here`);失败提示改成实话——
`ensure_project` 已经把目录建出来了,所以只有"连目录都没建成"的失败才会再问,提示不再一律声称会再问。

