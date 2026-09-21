# Plan 109 — web_fetch 保留全文，offload 指针给出可查询路径

## 触发：dogfood 实测

同一个 prompt（`审查：b8736765，956791f9`）在同一个仓库上跑三个 agent，kloop 是唯一漏掉三条高价值
finding 的一个，根因不是模型也不是推理，是 `web_fetch` 把证据丢了。

三方都要核 ElevenLabs 官方 schema：

- **claude code**：`curl -o el.json`（2.1MB 落盘，**0 上下文成本**）→ 6 段 `python3 -c` 查那个文件，
  只把答案带回上下文。据此拿到 `required` 集合、add-on 的百分比加价描述、body schema 没有
  `additionalProperties: false` —— 它的 3 条最高价值 finding 全部依赖这些。
- **codex**：沙箱 `network_access=false`，DNS 失败，拿不到。
- **kloop**：`web_fetch` 抓了同一个 URL 两次（会话内 247s、3205s），每次只留下 50,000 字符，
  进上下文的只有 2,234 字符 + 一个 `off-NNNN` 指针。

实测该 URL 是 **2,106,298 字节**。kloop 的天花板是它的 **2.4%**，而且是开头那 2.4%——
`components.schemas` 在很后面。kloop 结构上够不着，最后把"缺 `audio_duration_secs` → 502 +
suppress fallback"写进了**"已验证的正确部分"**，而 claude code 判它是最高危 bug。

## 缺陷定位

`crates/web/src/fetch.rs::read_body` 在 `kloop-web` 内部把转换后的正文裁到
`limits::MAX_TEXT_CHARS = 50_000` 并**丢弃其余**。

这一刀是多余且有害的：下游 `History::record` 本来就会把超过 `OFFLOAD_CAP_CHARS` 的 tool result
整篇 spill 到 session store，只把 head/tail 预览 + 指针留在上下文里（`history.rs:114-138`、
`spill()`）。也就是说 **offload 缝已经在做"限制模型可见量"这件事，而 50k 那一刀销毁的正是
offload 缝存在的意义**——被裁掉的部分连磁盘上都没有，`read_offloaded` 也救不回来。

第二个缺陷是可用性：即使全文进了 offload，`read_offloaded` 每次只给 24k 字符窗口，2.1MB 要 88 次
往返。指针只说 `use the read_offloaded tool`，**不给文件路径**，模型不知道可以用 grep/read_file/
run_program 直接查那个文件。

## 改动

1. **`crates/web/src/fetch.rs`**：去掉 50k 模型文本裁剪。5 MiB 下载上限保留（它约束的是传输），
   `[download truncated at 5MB]` 标记保留。模型可见量交给 offload 缝独家负责。
   `web_search` 的 50k 不动——它的输出是 kloop 自己从固定 5 条结果合成的，不存在"大工件被销毁"
   这个问题。

2. **`crates/core/src/history.rs::spill`**：指针带上字符数与 offload 文件的绝对路径，并明确指出
   大工件应该用 grep/read_file/run_program 去查，只让抽出来的部分进上下文。保留 `id=off-NNNN`
   与 `read_offloaded` 两个既有 token（现有测试按它们解析）。
   路径给的是**进程内读者**能用的路（`read_file`/`grep`/`run_program`）；`~/.kloop` 在 sandbox 的
   deny-read 里，沙箱内的 bash 读不到（plan 105 已接受的副作用），所以提示里不提 bash。

## 不做

- 不动 `web_search` 的 50k 上限（理由见上）。
- 不动 `OFFLOAD_CAP_CHARS` / `OFFLOAD_WINDOW_CHARS`。
- 不改 prompt 层去推动 `run_program` 被用起来。同一次 dogfood 里 `run_program` 用了 **0 次**，
  而 `crates/codemode` 早就是 codex `exec` 那套（"Intermediate results stay in program variables;
  only what the program returns flows back to the model"）——能力在，模型没去拿。这是独立的一条，
  留给后续 plan。

## 实测过程中长出来的第三处改动

计划里只有上面两条。四次 `--headless` 真实验收把第三条逼了出来：

- **第 1 次**（指针只说 "that file"）：正确答出 required 集合，但路径是靠 `bash` + python
  **重新下载**拿到的；期间试过 `grep {glob:"off-0001.txt"}`——glob 搜的是工作区,自然没命中。
- **第 2 次**（指针改成显式 `path="…"`）：**更差**。它拿对了绝对路径去调 `read_file
  {offset:0, limit:200}`——但 `read_file`/`grep` 是**行**导向的,而这份 spec 是 210 万字符的
  **一行** JSON,切不出任何东西;随后放弃文件、转去 web_search,打满轮次没答。
- **第 3 次**（指针补上"一行文档切不动"的形状说明）：**最差,而且是本 plan 自己造成的回归**。
  模型改用 `read_offloaded` 从 0 一路翻到 2,088,000，**约 88 次**,轮次预算全烧在翻页上。
  改动前正文只有 50k(2 个窗口),翻页是廉价的;正文变成 2.1MB 之后,`read_offloaded` 每次都
  照旧打印"call read_offloaded again with char_offset=N"——那句邀请变成了一条 88 步的跑步机。

于是第三处改动：**`read_offloaded` 的续行提示按剩余大小分叉**（`tools/fs.rs`）。剩余不超过
`MAX_INVITED_WINDOWS`(4) 个窗口时照旧点名下一个 offset;超过则**不再给那个数字**,改为报出还需
多少次、并指向磁盘上的文件。教训 93(a) 的同一条纪律:逃生口必须在它真正被使用的尺寸上有效,而
"能终止"不等于"值得走"。

- **第 4 次**（带 anti-paging 护栏）：翻页螺旋消失,模型直接转向 `bash`/`run_program` 查文件。
  但仍在 `run_program` ↔ `web_fetch` 之间反复、打满轮次没收敛。

## 验证

- `cargo fmt --check` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace`（退出码 0）
- 新增/改写测试：
  - web：超过 50k 的正文原样返回、不再有裁剪标记；下载上限仍独立报告且被下载的部分全保留。
  - core：spill 指针含字符数、绝对路径与 `path="…"` 形式，且该路径上的文件与原文逐字节相等。
  - core：`read_offloaded` 在剩余窗口数超阈值时不再给出下一个 offset，改为指向文件。
- 真实端到端（`--headless`，网关 `gpt-5.6-sol`）：`web_fetch` 该 URL 后落盘
  **2,111,302 字节**，`json.load` 可解析，`SpeechToTextChunkResponseModel.required` /
  `MultichannelSpeechToTextResponseModel.required` 都能读出来——**这正是改动前结构上够不着的事实**。

## 结论：修好了一半，另一半明确未修

- ✅ **证据不再被销毁**：`web_fetch` 的正文完整落盘、可解析、可查询。这是本 plan 的目标，已达成。
- ✅ **本 plan 引入的翻页回归已堵**。
- ❌ **模型仍不能稳定地一次就用好这份磁盘工件**：四次实测里只有第 1 次答对，且走的是重新下载。
  这属于 prompt / 工具可供性层——和"`run_program` 在原始 dogfood 里用了 0 次"是同一个问题，
  需要独立 plan 和自己的真实验收循环，不在本 plan 范围内。

## 完成

✅ 2026-09-02（部分：见上面的"结论"），提交 SHA 以本条所在提交为准。
