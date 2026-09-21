# Plan 148 — 降级的发现,不该躺在"排除"堆里

> 来源:2026-09-15,用户贴来一份 kloop `code-review` 跑出的报告(审的是一个 Go 仓库),
> 尾巴是一节 "降级 / 排除",9 条混在一起,问「能拆开吗」。

## 一、为什么会合起来

不是模型随手合的,是 skill 自己前后矛盾。`rust/crates/core/src/skills/code-review/SKILL.md`
里两处对同一件事给了相反的指令:

| 位置 | 原文 | 含义 |
|---|---|---|
| 裁决表 | `**downgraded** — real, but smaller than it first looked. Report it at its true severity` | 进正文 |
| 同节末 / `## The report` | `The report ends with the downgraded and excluded ones` / `Then downgraded and excluded` | 进尾巴 |

两条都要遵守,唯一的解就是在尾巴上写一节 "降级 / 排除" —— 每次都会这样,和模型没关系。

## 二、合起来的代价

用户那份报告的第一条是 `async-jobs.md` 的文档言过其实(durable 行不落 snapshot,文档说落),
**这是真缺陷**——SKILL.md 自己第 52-54 行就把"仓库里的规则文件现在与代码矛盾"算作 finding。
它被写在了另外 8 条"不是缺陷"中间。

降级项是**要动手的**,排除项是**证明覆盖过的**;两种读法混在一节里,前者必然被后者稀释——
读者扫一眼"我查过 X,没事",整节就过去了。

## 三、做什么

`SKILL.md` 三处 + 一条测试:

- 裁决表的 downgraded 条目:明写它进 findings 列表,并写清**为什么**(放进尾巴等于让一个
  要动手的缺陷坐在一排"checked, not a defect"里,读者把它当成其中一条),外加一条分界线
  ——**没人需要动手的,裁决本来就是 excluded,不是 downgraded**。这条分界线是必要的:
  不然"降级"会变成"排除"的软说法,合并只是换个地方发生。
- 同节末:`The report ends with the excluded ones.`(尾巴那节存在的理由一字不动)。
- `## The report` 开头:findings 按严重度排,降级项**在其中**,然后才是 excluded 列表。
- `skills.rs` 的 `code_review_keeps_its_load_bearing_clauses` 补一条
  `"not a softer word for excluded"`,按本仓惯例把测量到的症状写在注释里。

## 四、非目标

- **不动尾巴那节存在的理由**。"I checked X, it is fine because Y" 仍是唯一挡在
  "调查过的候选"和"悄悄消失"之间的东西(plan 125)。
- **不动 excluded 的证据标准**(exclusion 不需要 finding 那份实验开销,plan 126)。
- 不碰审查流程的其它部分。

## ✅ 已完成(2026-09-15;提交 SHA 以本条所在提交为准)

SKILL.md 三处如上,`skills.rs` 补一条 clause 断言,`rust/DESIGN.md` 的 builtin 段同步
(降级项进 findings 列表、不进 excluded 尾巴)。

### 验证

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --all-features
-D warnings`、`cargo test --workspace` 三条各自单独跑并当场取退出码。
