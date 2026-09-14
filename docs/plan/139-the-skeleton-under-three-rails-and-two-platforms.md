# Plan 139 — 骨架抄了三遍(和两遍)

> 来源:plan 136 的全仓通读,第五节非目标里的两条(二.7 SSE 驱动 / 二.3 `atomic_replace`)。
> 2026-09-11 用户要求把 136 的非目标排成计划;本 plan 是其中一条,与 138、140–142 同批,
> 可独立开工。**读完即可动手。**

两件事放在一条 plan 里,因为它们是**同一个形状**:真正因平台/协议而异的只有两三个调用,
却各自拖着一副一模一样的骨架,而骨架里全是安全检查和资源上限——最不该分叉的那种代码。

## 一、SSE 驱动:三条 rail 各写一遍

`crates/provider/src/{anthropic,openai,responses}.rs` 的 `stream()` 开头完全一致:

```
send_checked(req, rail, key)         →  resp
SseParser::default()                 →  parser      (anthropic:269 openai:409 responses:919)
GuardedBody::new(resp.bytes_stream())→  byte_stream (…:270      …:410      …:920)
loop { byte_stream.next().await? → chunk; for frame in parser.feed(&chunk)? { … } }
parser.finish()?                                    (…:473      …:531      …:1321)
```

三家真正不同的只有 `for frame` 里那个事件状态机。骨架里却有:空闲/墙钟超时、响应字节
上限、帧大小上限、UTF-8 整帧解码、EOF 残留分类(`finish()` 区分"非法 UTF-8 = 协议违规
不可重试"与"未终止的帧 = 可重试")。这些是 plan 64 一条一条立起来的判断,现在有三份。

**做法**:在 `provider/src/stream.rs`(骨架的其余部分已经住在那里)加

```rust
/// Drive one SSE response to completion, handing每个完整帧给 `on_frame`.
/// The rail keeps its state machine; the guards (timeouts, byte and frame
/// caps, whole-frame UTF-8, EOF residue classification) live here once.
pub(crate) async fn drive_sse<F>(
    resp: reqwest::Response,
    mut on_frame: F,
) -> Result<(), ProviderFailure>
where
    F: AsyncFnMut(SseFrame) -> Result<ControlFlow<()>, ProviderFailure>,
```

`ControlFlow::Break` 让 rail 在收到终止帧后停下(`openai.rs` 的 `finish()` 在块里、不在
函数末尾,原因就是它中途会 break——改造时先确认这一点,见第三节)。

`AsyncFnMut` 需要 Rust 2024 的 async closure;工作区已是 edition 2024 / rust-version
1.96,可用。若借用检查在某条 rail 上卡住(状态机同时可变借用多个局部),退路是把该 rail 的
状态收进一个小 struct 再传进去,**不要**退回复制骨架。

## 二、`atomic_replace`:unix / windows 两份逐行相同

`crates/core/src/tools/fs.rs:1383`(unix)与 `:1493`(windows),各约 80 行,顺序完全一致:

```
100 次 temp 名重试循环(TEMP_SEQUENCE + pid)
  ├ 建 temp                     ← 平台:rustix::openat(O_CREAT|O_EXCL|O_NOFOLLOW) / windows::create_temp_file
  ├ write_all / set_permissions / sync_all
  ├ #[cfg(test)] 故障注入
  ├ verify_target_unchanged     ┐
  ├ verify_parent_binding       ├ 三个安全检查,两份一字不差
  ├ verify_temp_binding         ┘
  ├ rename                      ← 平台:rustix::renameat / windows::rename_file_relative
  └ sync_parent
失败时清理 temp                  ← 平台:rustix::unlinkat / windows::delete_file_handle
```

平台相关的只有三个动作。第三份 `:1583` 是 `not(any(unix, windows))` 的 `bail!` stub,保留。

**做法**:定义一个三方法的内部 trait(或三个 `#[cfg]` 自由函数),骨架写一次:

```rust
/// The three steps a platform has to answer for; everything else about an
/// atomic replace — the retry loop, the three verifications, the sync — is
/// the same on both, and used to be the same twice.
fn create_temp(parent: &File, name: &OsStr) -> io::Result<File>;      // EEXIST 要能分辨
fn commit_rename(temp: &File, parent: &File, name: &OsStr, leaf: &OsStr, replace_existing: bool) -> io::Result<()>;
fn discard_temp(temp: &File, parent: &File, name: &OsStr);
```

注意 windows 版 `rename_file_relative` 多一个 `replace_existing: expected_target.is_some()`
参数,unix 的 `renameat` 无条件替换——这个差异**要保留**,所以它进签名。

## 三、可能的坑(实现时先验证,别假设)

1. **`openai.rs` 的 `finish()` 在块内**(531 行,有缩进),另两家在函数末尾。先读懂它为什么
   在那里,再决定 `ControlFlow` 的语义;如果它其实是提前 return 的路径,`drive_sse` 的契约
   要能表达"rail 主动收尾"。
2. **`responses.rs` 的 `stream()` 有 425 行**,是三家里最复杂的(plan 95 的 out-of-band
   事件、plan 96 的 arguments JSON 等价)。建议**先改 anthropic(最短的完整状态机),跑通
   `crates/provider/tests/anthropic.rs`,再搬另外两家**。
3. **`GuardedBody` 的 idle 计时在 `next()` 里重置**。抽出去之后这个语义不能变——
   `stream.rs` 已有 `body_distinguishes_idle_and_wall_timeouts` 锁着它,改完必须仍绿。
4. **fs 那边的 `#[cfg(test)]` 故障注入**(`CommitFault::BeforeRename` / `ReplaceTempName`)
   只有 unix 版有第二种。骨架统一后 windows 版会**获得**它,这是对的:两个平台跑同一套
   安全检查,就该能被同一套故障注入检验。`ReplaceTempName` 现在的实现用了
   `std::fs::remove_file` + `std::fs::write`(按路径,不按 fd),在 windows 上要换成等价的
   句柄操作。**不要**用 `#[cfg(all(test, unix))]` 把它按回原样——那是让统一只统一了一半。

## 四、非目标

- 不动三条 rail 的**事件状态机**,一行都不动。本 plan 只搬骨架。
- 不动 `SseParser` / `GuardedBody` / `send_checked` 本身的行为。
- 不动 `fs.rs` 的三个 `verify_*`,它们是 plan 49/66 的产物,本 plan 只让它们少一份副本。

## 五、验收

- `crates/provider/tests/{anthropic,openai,responses,effort_probe}.rs` 全绿且**一条都没改**
  ——这是"骨架搬家没改语义"的唯一证据;
- `fs.rs` 的既有 mutation 测试全绿,同样不改;
- 两处的行数净减(SSE 约 −60,fs 约 −70),且 `grep -c 'SseParser::default()' ` 从 3 变 1;
- fmt / clippy(`-D warnings`) / `cargo test --workspace` 各自单独跑、当场取退出码。
