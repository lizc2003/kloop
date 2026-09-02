# Plan 110 — 大工件走 code mode：让建议先成立，再让模型找得到

## 承接

Plan 109 修好了"证据不再被销毁",但明确挂账了另一半:**模型不能稳定地一次就用好那份磁盘工件**。
四次真实验收里只有第一次答对,且走的是重新下载。

随后单独测了机制本身,**一次就过、两轮答对**:

```
[run_program {source:"const r = await tools.web_fetch(...); const d = JSON.parse(r); ..."}]
[web_fetch {...}]            ← 程序内部那次调用
{"SpeechToTextChunkResponseModel":[...], "MultichannelSpeechToTextResponseModel":["transcripts"]}
```

2.1MB 在 JS 变量里 parse,只 return 两个数组,正文一个字节没进上下文。**点名就行,不点名就不行**——
所以这一半是选择问题,不是能力缺失。

## 先修一个我自己说错的前提

Plan 109 的指针写的是"query the file at path=… : grep or read_file …"。查下来 **`read_file` 有
`READ_CONTENT_CHARS = 30_000` 上限,在程序里同样生效**;而这份 spec 是一行 210 万字符的 JSON,
会撞上 `fs.rs:464` 那条 `[read output truncated within line N; use grep or a narrower reader]`
——对一行文档来说是死路。也就是说:

**目前没有任何"在程序里把大文件整个读进变量"的路径。** `read_file` 截 30k,`read_offloaded` 一次
一个 24k 窗口。程序唯一能拿到全文的办法是重新 `tools.web_fetch`(白跑一次网络)。

指路之前得先让那条路存在。

## 改动

1. **`read_offloaded` 在程序里返回全文**(`tools/fs.rs`)。窗口是**上下文**约束,不是数据约束——
   它存在的唯一理由是"回复要进模型上下文,过大会被再次 offload"(见 `OFFLOAD_WINDOW_CHARS` 的文档
   注释)。程序里的返回值进的是 JS 变量,没有那个约束,所以 `ctx.from_program` 为真时不开窗、
   不加续行提示、直接给全文。这与 plan 27 让 `from_program` 跳过 deferred `locked()` 门是同一条
   推理:那也是一个**发现**门而非安全门,这里是一个**上下文**门而非数据门。
   权限/hooks/沙箱一概不变。

2. **指路改为以 `run_program` 领衔,并给出可照抄的调用形状**(`history.rs` 的 spill 指针 +
   `tools/fs.rs` 的超阈值重定向)。Plan 109 把 `run_program` 排在四个选项的末尾当兜底,而它是唯一
   真正能干这活的;现在头条是
   `run_program` + `tools.read_offloaded({id})` + 在 JS 里抽取。grep/read_file 降为"行结构文本"
   的次选,并保留一行说明它们切不动一行文档。

3. **`web_fetch` 的描述加一句预防**(`tools/web.rs`)。最好的结果是正文根本不进上下文:预期文档很大
   时,把 fetch 放进 `run_program`,正文只活在 JS 变量里(`CoreBridge::call_tool` 走 `run_one`
   拿到结果后直接交给变量,**绕过 History 的 offload**)。这比"落盘之后再想办法查"早一步。

## 不做

- **不收窄工具面**。三方对比里 claude code(44 次全 Bash)和 codex(唯一工具是 JS runtime)的共同点
  是"工具面只有一个",kloop 是 15 个 + `run_program`;但那是 plan 24 已经拍过板的架构决定
  (`run_program` 作为其中一个内置工具),而且现有证据是**每家一次**的 dogfood、三家模型还不同,
  不足以翻案。先用本 plan 的引导做几轮真实验收:如果一引导模型就稳定切过去,说明激励差距没有
  结构性到那个程度。
- **不动 `read_file` 的 30k 上限**。同一条"上下文门不该管数据"的推理对它同样成立,但它的面大得多
  (offset/limit 语义、image、notebook),够单独一片。**已知相邻陷阱,记在这里**:程序里
  `tools.read_file` 读大文件仍只得 30k。

## 实测结果

三次 `--headless`,**都不点名任何工具**:

1. schema required 问题 → `web_fetch` → **`run_program`** → 答对。**3 轮**。
   (对照 plan 109 同题:四次里只有一次答对,且走重新下载,其余打满轮次。)
2. 四个 add-on 加价百分比 → 工具选择对了(立刻走 `run_program`),但**程序内反复重新
   `tools.web_fetch` 同一个 2.1MB**、失败三次,再退回 `bash` + 7 次 `web_search`,最后从搜索
   结果凑出答案。**修好了选择,暴露了新的浪费。**
3. 同题重跑(见下面的第四处改动)→ `web_fetch` 一次 → **`run_program` +
   `tools.read_offloaded({id})`** → 答对。**一次网络往返,2.1MB 全程不进上下文。**

## 第四处改动:两条提示互相打架

第 2 次暴露的是我自己造的问题:`web_fetch` 的描述说"大文档就在 run_program 里抓",offload 指针说
"用 run_program + read_offloaded 读回来"——**对同一个已经抓过的正文,两条建议冲突,而错的那条赢了**,
于是每次重试都重新拉 2.1MB。

改法是划清各自的管辖:`web_fetch` 的那句只管**首次**抓取,并明确"已经 offload 过的正文不要再抓一遍,
在程序里用 `tools.read_offloaded` 复用,它的指针写了怎么调"。第 3 次实测即是这条生效后的形状。

判据:**给模型的多条提示要按"适用时机"划分管辖,不能只各自正确。**两条都对但都常驻,模型只会挑先看到
的那条,而"先看到"和"该用"没有关系。

## 第五处改动:改了行为却没同步描述

用户问"模型怎么区分 read_offloaded 和 read_file",顺着查出一个我自己留下的口子:
`read_offloaded` 的描述还写着"Returns one window of it",**没提 `from_program` 下返回全文**。
而 `run_program` 的 TypeScript API 正是从这些描述生成的——程序里的模型读到的仍是"返回一个窗口",
要么白循环,要么去另找工具。第 3 次验收之所以跑通,靠的是 offload 指针那句话,不是工具描述。

补上之后描述同时说明两种行为,并点明与 `read_file` 的分工(**吃 id 不吃 path**;`read_file` 面向
工作区文件、按行分页,切不动一行文档)。

判据:**改了一个工具在某条路径上的行为,必须同步它的描述——尤其当那段描述会被生成进另一个工具的
API。**行为与描述分叉时,能跑通只说明别处有一句提示在兜底,不说明模型理解了这个工具。

## 两个 read 工具的分工(存档)

形式上的判据是 **id vs path**:`off-NNNN` 只能从 offload 指针抄到,模型造不出来,所以正常不会混。
Plan 109 破坏这个区分的方式是**指针同时给了绝对路径**——同一个对象两个入口,而它们在这个对象上
并不等价(`read_file` 预算 30k、offset/limit 是行,遇一行 JSON 死路)。Plan 110 把管辖收回:
指针以 `read_offloaded({id})` 领衔,path 降为"仅当行结构文本"的次选。

## 验证

- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace`(退出码 0)
- 新增回归:`read_offloaded` 在 `from_program` 下返回全文且不带续行提示,同一测试里非程序路径的
  窗口/续行/超阈值重定向三条行为逐字不变;指针与重定向文案含可照抄的
  `run_program` + `tools.read_offloaded({id:"…"})` 调用形状。
- 真实端到端:上面三次。

## 诚实的样本量

行为类结论是 **n=3**(其中修好之后 n=2),单模型(gateway `gpt-5.6-sol`)、单场景(大 JSON 工件)。
"引导能让模型稳定切到 code mode"这个判断在这个范围内成立,**没有**推广到别的工件形状、别的模型或
别的任务类型。plan 109 里"不收窄工具面"的理由不变——但现在多了一条正面证据:引导这一档确实有效,
不必先跳到架构改动。

## 完成

✅ 2026-09-02，提交 SHA 以本条所在提交为准。
