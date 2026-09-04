# Plan 118 — 工具结果要讲清楚剩下什么，别让模型靠猜

> 来源：Plan 117 的 dogfood 数据（`审查：cc0a236b，951a19cb` 三家对比）留下的两条
> 候选，加上同一份 rollout 里 web_fetch 的那段弯路。三条问题同一个形状：
> **工具把话说了一半，模型只能再发一轮去补**——而在 kloop 这里，一轮试探
> = 一次采样 + 一次上下文增长（实测每轮中位数 +10,664 token）。

## 一、`read_file` 知道文件有多长，却不告诉模型

`fs.rs:357` 已经算出 `total_lines`，只用来做越界警告。返回给模型的正文里没有它。

截断提示只在**字符预算**超了时才打（`fs.rs:463-470`）；模型自己给的 `limit` 用完时
`observed_end == requested_end < total_lines`，**一个字都不说**。模型于是不知道自己
只看了半个文件。

实测后果（plan 117 那次会话，主 agent 26 轮）：

- `upstream/elevenlabs/batch.go` 被 `read_file` 读了 **7 次**
- `gateway/orchestrator/async_submit.go` 3 次——其中一轮里同时发了
  `offset 1 limit 220` 和 `offset 1 limit 260` 两个**完全重叠**的调用
- 典型序列是 limit 一路加码：220 → 260 → 520，430 → 620

**改动。** `numbered_text_page` 收下 `total_lines`，出口统一成一句：

- 还有剩余（无论是字符预算还是 limit 截断）：
  `[showing lines X-Y of N; call read_file with offset=Y+1 to continue]`
- 行内截断保持原语义，补上总数：
  `[read output truncated within line L of N; use grep or a narrower reader]`

读全了就什么都不加——完整读取不该在结果里留噪声。现有测试断言了旧文案
（`fs.rs:1922`），跟着改。

## 二、`grep` 的 `context` 是 kloop 自己加的第五个同义参数

schema 里控制上下文行数的有四个：`-A` / `-B` / `-C` / `context`
（`tools/mod.rs:824-828`），而 `search.rs:92` 的解析是
`context.or(-C)`，再 `.or(-A)` / `.or(-B)`——**三层覆盖**。

cc 的 Grep 只有 `-A`/`-B`/`-C`；`context` 是 kloop 多出来的别名，它的描述还得靠
一句「overridden by context」去解释另一个参数的存在。模型的反应是把能填的全填上：
那次会话里几乎每次调用都是 `{-A:15, -B:15, -C:15, context:0, -i:false, -n:true,
-o:false}`——**七个开关，且 `-C:15` 和 `context:0` 自相矛盾**。

**改动。** 删掉 `context`，只留 `-A`/`-B`/`-C`（对齐 cc 与 ripgrep 惯例），
`-C` 的描述里那句「overridden by context」一并去掉。`search.rs` 的 `around`
只读 `-C`。用了 `context` 的测试改成 `-C`。

**取舍**：反过来「删 `-A`/`-B`/`-C` 只留 `context`」也能瘦身，但那是往远离 cc 的
方向走，且 ripgrep 的短名是模型的肌肉记忆。

## 三、`web_fetch` 把「这页需要 JavaScript」当正文交了出去

`fetch.rs:130` 抽完正文，非空就原样返回。客户端渲染的站点抽出来是一句占位符，
它非空，于是模型收到的是：

```
You need to enable JavaScript to run this app.
```

模型无从判断这是页面内容还是抓取失败。实测那次会话对着火山引擎文档站
（`docs.volcengine.com`）连试两个 URL，拿到 redirect 提示和上面这句，然后退化成
**4–5 轮 web_search** 去猜官方 schema 长什么样。

**改动。** 抽取后做一次保守判定：正文 trim 后 < 200 字符、原始 HTML > 1000 字节、
且含 `<script`——三条同时成立才认为是客户端渲染的壳，在返回文本后追加一行明确信号，
说明正文不在 HTML 里、重复抓这个 URL 不会有别的结果、改用 web_search 或找静态镜像。

**不做的**：上 headless 浏览器真渲染。那是另一个量级的依赖，本片只把「失败」说清楚，
省掉模型盲试的那几轮。

## 四、附：修正 Plan 117 第四节关于 prompt cache 的结论

Plan 117 写的是「cache 命中率 20% 是 gateway 亲和不稳，kloop 侧无可修」。拿到 key
之后做了两组对照实验（脚本一次性的，未入库），结论要改写：

**实验 1**（同一请求连发 3 次，各自独立 cache_key）：

| 布局 | #1 | #2 | #3 |
|---|---|---|---|
| A 大块在 `instructions`（kloop 形状） | miss | miss | 98.9% |
| B 大块在 `input[0]` developer（codex 形状） | miss | 98.8% | 98.8% |

**实验 2**（前缀逐轮增长 8 轮，两种布局交替发出以抵消时间因素）：

| 布局 | 命中轮次 | 累计命中率 |
|---|---|---|
| A `instructions` | 5/7 | 67.1% |
| B `developer input` | 6/7 | 81.5% |

**结论：布局不是原因。** 两种形状都进缓存，命中时都是 90%+（A: 87.8/93.5/93.9/90.3/95.0，
B: 91.7/92.3/92.8/93.3/93.5/94.0）；差距全部来自 A 多 miss 了一次，7 轮样本里一次
miss 值 14 个百分点，区分不出系统性差异。**「kloop 把 system 放 Responses 的
`instructions` 字段所以进不了缓存」这个假设被证伪。**

但实验给出了基准：**这个渠道在受控条件下能稳定跑到 90%+**，而 kloop 真实会话只有
19.7%（codex 41.3%）。既然不是渠道、不是布局、不是 `prompt_cache_key`（在发），
落差只能来自会话形态：

- kloop 每轮 **+10,664** token（codex 3,706，实验里每轮只 +1k），26 轮撑满 223k 窗口；
- 压缩把整个前缀换掉，缓存从零重建，之后还重读了一遍已经读过的文件；
- 轮次少（26 vs 259），单次 miss 在小样本里权重极大。

所以缓存命中率是**结果**而不是病因，病因是每轮往上下文里塞多少——本片第一节
（`read_file` 不再引诱重读）正是冲它去的。

> **2026-09-04 补记（Plan 120）**：这个判断后来被 2019 轮采样证实，不再只是推测。
> 控制上下文规模后，单轮增量 0–2k 的命中率中位数 98%，>20k 的只有 14%。中间
> Plan 120 初稿曾以"相邻两轮 +145 token 也 0 命中"为由撤回过这一节的结论，那是
> 把因果搞反了（前一轮增量 33k 才是原因），已在 Plan 120 里更正。

## 验证

- `cargo fmt` + `cargo clippy --all-targets` + `cargo test` 全绿。
- `read_file`：新增/改写断言覆盖三种出口——读全（无尾注）、limit 截断（报总数与
  下一个 offset）、字符预算截断（同上）、行内截断（报总数）。
- `grep`：`context` 从 schema 和解析里消失；`-C` 仍生效且 `-A`/`-B` 可单独覆盖。
- `web_fetch`：占位符页面追加信号，正常短页面（无 script 或 HTML 本身就小）不误判。
