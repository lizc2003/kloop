# Plan 177 — 三个传输里,只有 stdio 没有自己的文件

> 本批来源:2026-09-21,plan 176 的棘轮落地后用户一句「我要的是现有文件的架构重整」。
> 批次判据与统一纪律见 `HANDOFF.md` 第〇节。**这条是本批最小的一个,建议先做**——
> 它的边界最干净,做完一次就知道这批的节奏对不对。

## 一、现状

`rust/crates/mcp/src/lib.rs`,**884 code 行 / 1164 总行**。crate 里已经有
`http.rs`(HTTP 传输)、`sse.rs`、`oauth.rs` 三个兄弟模块,**唯独 stdio 传输挤在 lib.rs**。
按顶层项分,这个文件现在住着四样东西:

| 行 | code 行 | 是什么 |
|---|---|---|
| 74–290 | 180 | 线类型与错误:`McpServerCapabilities` / `McpNotification` / `McpResource` / 五个 Error |
| 291–330 | 27 | `pub(crate) trait Transport` — 两个传输的公共契约 |
| 332–616 | 231 | `McpClient`:握手、分页 `tools/list`、`tools/call`、`validate_call_tool_result` |
| 617–960 | ~340 | **stdio 传输**:`PendingRequest`、`StdioTransport`、`publish_transport_state`、`fail_pending`、`write_line`、`read_bounded_line`、`read_loop` |
| 962–1058 | ~60 | **content 呈现**:`render_result` / `render_content` / `render_item` / `supported_image_mime` / `usable_image` / `content_blocks` |

后两样和"说 JSON-RPC 线协议"都不是一件事:一个是具体传输的实现,一个是把服务器返回的
content 数组渲染给人看/转成 `ContentBlock`。

## 二、切法

1. **`crates/mcp/src/stdio.rs`**(≈340 code)— 617–960 整段搬过去。
   `Pending` / `SharedWriter` 两个 type 别名(287–288)只有 stdio 用,一起搬;
   `Transport` trait 留在 lib.rs(HTTP 也 impl 它)。
2. **`crates/mcp/src/render.rs`**(≈60 code)— 962–1058 整段搬过去,`pub fn render_result`
   与 `pub fn content_blocks` 保持 `pub`(CLI 在用),其余三个降成模块私有。
3. lib.rs 剩下线类型 + `Transport` + `McpClient`,≈480 code 行。

`mod stdio;` / `mod render;` 都是私有 mod,`pub use` 保持 crate 对外 API **一个符号不变**。

## 三、坑

- **`#[cfg(test)] mod wire_tests`(1069–1164,9 code 行)覆盖的是 `read_bounded_line`**
  (`use super::read_bounded_line`)。它测的是 stdio 的分帧,**跟着 stdio 一起搬**,
  不然搬完够不到那个私有函数。
- `crates/mcp/tests/` 下的集成测试走的是公共 API,**一行不用改**;它们是这次重构的安全网,
  改动前后必须逐条通过。
- `supported_image_mime` 在 `crates/cli/src/mcp.rs:1903` 还有一份**同名同义的实现**。
  这次**不动它**(跨 crate 去重是另一件事,而且 cli 那份的判据可能已经漂移);
  只在本 plan 记一笔,别顺手合并。
- 搬进子模块后,`publish_transport_state` / `fail_pending` / `write_line` 这些文件私有函数
  如果 lib.rs 还要用就得提可见性——**先确认它们只有 stdio 用**(现状是),
  确认了就保持模块私有,别顺手改 `pub(crate)`。

## 四、验收

- `make check` 全绿;`crates/mcp/tests/` 的每条集成测试通过。
- `mcp/src/lib.rs` 降到 ≈480
  (低于阈值的行会被摘掉),这一步要写进提交信息。
- crate 的 `pub` 符号表一个不少不多。

## ✅ 完成(2026-09-22)

一次提交(SHA 即本条所在提交),`make check`(fmt + clippy -D warnings + 全量测试)全绿,
`crates/mcp/tests/client.rs` 的 14 条集成测试一行没改、全过。

**切完的落点**(第二节的三块,行数与预估基本吻合):

| 文件 | code 行 | 是什么 |
|---|---|---|
| `lib.rs` | 884 → **481** | 线类型与错误 + `Transport` trait + `McpClient`(握手、分页、tools/call 校验) |
| `stdio.rs` | — → **434** | stdio 传输本体,连同它的 `#[cfg(test)] mod wire_tests` |
| `render.rs` | — → **90** | content 数组 → tool_result 文本 / `ContentBlock` |

`Pending` / `SharedWriter` 跟着 stdio 走;`mod stdio;` / `mod render;` 都是私有 mod,
`render_result` 与 `content_blocks` 用 `pub use` 原样再导出——对外符号表前后逐条核对过,
**一个不少不多**(`http.rs` 测试里的 `crate::render_result` 也因此一个字没改)。

**第三节四个坑的实际答案**:

1. `wire_tests` 确实跟着搬(它现在叫 `stdio::wire_tests`)。它 `use super::X` 够到的
   `McpTransportFailure` / `McpTransportState` 这些,现在是 `stdio.rs` 顶上的 `use crate::X`
   ——私有 `use` 对子模块可见,所以一行没改。
2. 集成测试一行没改,全绿。
3. `supported_image_mime` 在 CLI 那份**没动**,按 plan 记一笔:跨 crate 去重等 184 之后另算。
4. **可见性只提了三个符号**:`StdioTransport` 与它的 `spawn` / `over` 提到 `pub(crate)`
   ——因为调用方 `McpClient` 在父模块。`publish_transport_state` / `fail_pending` /
   `write_line` / `read_bounded_line` 确认只有 stdio 用,全部保持模块私有;反方向
   (stdio 够根上的 `MAX_WIRE_MESSAGE_BYTES`、`Transport`、`McpTransportHealth::healthy` 等
   七八个私有项)**一个都不用提**——见教训 165。

**第四节第二条已失效**:体积棘轮(`architecture-baseline.toml` + 门禁)已于 2026-09-21
整体删除,没有"低于阈值的行会被摘掉"这回事了,提交信息里只写降到了多少。
