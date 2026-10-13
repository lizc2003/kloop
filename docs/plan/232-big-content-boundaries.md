# Plan 232 — 大数据内容的三个边界 ✅

> 2026-10-13。用户要求按参考实现处理大数据内容：标记说清规模、统一三处尺寸边界的
> 语义、给结构化内容形状提示。全部改动集中在 `crates/web` 与 core 的 offload 缝。
> 实现提交 SHA 以本文件所在提交为准。

## 参考结论（取证来源）

- **codex**（公开 OSS）：`TruncationPolicy = Bytes | Tokens` 由模型配置、工具共用
  （`codex-rs/protocol/src/protocol.rs:3485`，用于 `tools/src/tool_call.rs:153`）；
  `formatted_truncate_text`（`utils/output-truncation/src/lib.rs:23-34`）在正文**前面**
  给出原始 token 数与总行数；`truncate_text` 留中间（`truncate_middle_*`）；结构化结果
  按条目截断（`[omitted N text items …]`）；`with_serialization_allowance` 把序列化
  开销计入预算。
- **claude-code**（parity 语料）：大输出标记是"输出过大（带**实际大小**）+ 完整输出
  已保存到 <路径>"——即超限内容应当**存在于某处**；kloop 的 core 层照搬了这个形态，
  fetch 层没有（超限部分被删）。
- **ZCode**（本地 refs）：按工具的模型可见上限（100k–120k）配更大的原始上限；缓存按
  字节预算。路线与 kloop 的 bet 1 相反，只借了缓存形状（不在本计划内）。
- **pi / deepseek-harness** 在这条缝上没有实现；**grok-build** 的实现不在克隆里
  （只有 UI 与配置引用）。

## 三项改动

1. **两处标记改成 codex 形状。** core offload 指针改为**规模前置**：第一行给出
   `chars`、`lines`（行数正是就地切片 `sed`/`python3 -c` 所需的刻度）与路径，中间标记
   带上省略量 `…[N chars truncated]…`。保留既有 `saved to` 词汇与 `POINTER_END` 收尾
   ——`is_offload_pointer` 与 request reduction 依赖后者，多个测试按前者取路径。fetch
   的传输截断标记改为按 `limits::MAX_DOWNLOAD_BYTES` **生成**（原先是写死的 "5MB"，
   会随常量漂移），并写明剩余内容这个工具取不回、重复抓取只会停在同一上限。
2. **三处尺寸边界的语义统一。** fetch 上限 5 → **20 MiB**、`FETCH_TIMEOUT` 30 → **90s**，
   两者**成对**移动：`Client::timeout` 覆盖含读体的整个请求，只抬字节会让慢链路先超时，
   而超时丢掉的是整个正文（比截断更差）；90s 仍在 core 给外部工具的 300s 预算内
   （`tools/mod.rs:149`）。语义写进 `limits.rs` 注释与 DESIGN：**工具内联层是安全界**
   （可能裁掉，且必须高于正常目标所需）、**core offload 是唯一保留工件的一层**、
   search 的 50k 裁的是就地合成结果（没有工件可失）。
   **Range 续传推迟**：它是 >20 MiB 目标的唯一正解，但要给 schema 加字节区间字段、并让
   每个区间请求重走 SSRF 门禁，属于接口表面积，等确有需求再做。
3. **结构化内容给出形状提示。** core 的落盘预览增加一行：内容 ≤ 4 MiB 且去空白后以
   `{` / `[` 开头时用 `serde_json` 解析，输出 `JSON array: N items` 或
   `JSON object: N keys: k1, …`（键名上限 12，其余标 `… +N`）；解析失败（常见于被截断的
   JSON）或超过上限则不解析、不加这一行。行式数据（NDJSON / CSV）由第 1 项的行数覆盖
   （行数即记录数），不单独解析。

## 不做的

- Range 续传（理由见上）。
- bash 前台输出上限 `FOREGROUND_OUTPUT_CAP_CHARS`：另一个工具，其标记本就量化了省略量。
- 模型可见字符上限、摘要模型（与 bet 1 冲突，只作对照）。

## 复核后修订（2026-10-13）

- **形状行未限制键名长度，指针本身会超上限**：`MAX_KEYS` 只限数量，`keys.join(", ")`
  仍完整复制每个键，而键名是任意长度的对端文本。一个键名长达 20 万字符的 JSON 对象
  能通过 4 MiB 解析门槛，产出的落盘指针远超 32,000 字符；`spill()` 直接接受它，它又低于
  128,000 的轮预算，于是整段进入历史——**指针取代的正是因超限才落盘的那个结果，反而
  突破了那个上限**，而 request reduction 还会因它是 offload 指针而跳过缩减。修法：单键
  40 字符 + 整行 600 字符两道界（新增 `clipped`），并补两条回归——长键对象的形状行在界内、
  以及落盘指针自身必须低于 `OFFLOAD_CAP_CHARS`。
- 该回归里我最初把断言写成"长键排在首位"，实测失败：serde_json 的 map 是排序的
  （BTreeMap），改为不依赖键顺序的断言。

## 验证

- `cargo test -p kloop-core history::`：32 项通过（新增指针首行的 chars/lines/path 断言、
  `shape_hint` 的数组/对象/键名上限/非 JSON/超限不解析用例，以及复核后补的长键对象与
  "指针低于触发上限"两条回归）。
- `cargo test -p kloop-web`：32 项通过、1 项 `#[ignore]`。20 MiB 的端到端用例（传输截断）
  改为按需显式运行——`cargo test -p kloop-web -- --ignored`，已单独跑过一次通过；门禁不再
  为它付 20 MiB 的代价。"标记随常量生成、不漂移"这条件由新的纯函数用例
  （`the_truncation_marker_names_the_bound_and_says_the_rest_is_gone`）覆盖。
- `cargo test -p kloop-core agent::tests`：142 项通过（按 "saved to" 解析路径的既有用例
  仍然成立）。
- 外层执行沙箱之外 `make check` 全绿：fmt、clippy、workspace debug 全量测试、本机 parity。
- `git diff --check` 与暂存后 `git diff --cached --check`。
- 无真实模型样本：标记与形状行的变化不做真实模型再验收，不声称因此改善完成度。