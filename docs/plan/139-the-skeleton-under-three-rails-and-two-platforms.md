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

---

## ✅ 已完成(2026-09-14;提交 SHA 以本条所在提交为准)

两处骨架各自收成一份。三条 rail 的事件状态机、`fs.rs` 的三个 `verify_*`,一行没动。

### 一、SSE 驱动:`drive_sse` 的形状被 stable Rust 否了,改成 `SseFrames`

plan 第二节给的签名(`drive_sse(resp, on_frame)`,`F: AsyncFnMut(SseFrame) -> …`)**在
1.96.1 上编不过**,而且卡点不是 plan 预判的借用检查:

```
error: implementation of `Send` is not general enough
   --> lib.rs:541  spawn_stream(move |sink| async move { anthropic::stream(…, &sink).await })
   = note: `Send` would have to be implemented for `&'0 StreamSink`, for any lifetime `'0`…
```

`AsyncFnMut` 的调用 future 是高阶(`for<'a>`)的,auto trait 泄漏对它失效,于是整条
`spawn_stream` 的 future 证不出 `Send`。唯一的修法是给调用 future 直接加 `Send` 约束
(`for<'a> F::CallRefFuture<'a>: Send`),而 `CallRefFuture` 是 unstable
(`async_fn_traits`);最小复现里它还顺带把捕获推成 `'static`,闭包连捕获局部变量都不行。
plan 写的退路(把状态收进 struct 再传进去)治的是借用,治不了这个。

改用**拉取式**:`stream.rs` 的 `SseFrames<S>`(`new` / `stop` / `next`),rail 侧从

```rust
loop { let Some(chunk) = byte_stream.next().await? else { break };
       for frame in parser.feed(&chunk)? { … } 
       if 收尾 { return } }
parser.finish()?;
```

变成

```rust
while let Some(frame) = frames.next().await? { … if 收尾 { frames.stop(); } }
```

没有闭包就没有 HRTB,`continue` / `break` 也照常能用(openai 的 `[DONE]` 分支就是一个
`continue`)。守卫——空闲/墙钟超时、响应字节上限、帧大小上限、整帧 UTF-8、EOF 残留分
类——全部只剩 `SseFrames::next` 这一份。

**两条语义被原样保住,都不是显然的**:

1. **收尾之后,当前 chunk 剩下的帧仍然要交付。** 三条 rail 都靠这个 fail closed
   (anthropic 的 `semantic event arrived after message_stop`、responses 的
   `semantic event arrived after response terminal`、openai 的
   `SSE frame arrived after [DONE]`)。所以 `stop()` 只标记,`next()` 先把 `ready` 里
   已解析出来的帧发完,再返回 `None`。改成"立刻停止交付"会让这三条检查永远打不响。
2. **`parser.finish()` 只跑在读到 EOF 那条路上。** 主动停下来之后 buffer 里的残留是我
   们自己不读了,不是协议违规。

唯一一处**刻意的**语义变化:openai 收到 `[DONE]` 却没收到 `finish_reason` 时,原来会
再跑一次 `parser.finish()`,现在不跑(`[DONE]` 是主动停)。两条路的错误都是可重试的
`incomplete_protocol`,`done_without_finish_reason_is_not_completion` 覆盖着它。

### 二、`atomic_replace`:两份 80 行合成一份 + 四个平台钩子

`#[cfg(any(unix, windows))] fn atomic_replace` 一份,平台只回答:

| 钩子 | unix | windows |
|---|---|---|
| `create_temp(parent, name)` | `openat(O_CREAT\|O_EXCL\|O_NOFOLLOW)` | `windows::create_temp_file` |
| `commit_rename(temp, parent, name, leaf, replace_existing)` | `renameat`(总是替换,忽略该参数) | `rename_file_relative`(要被告知) |
| `discard_temp(temp, parent, name)` | `unlinkat` 按名字 | `delete_file_handle` 按句柄 |
| `temp_name()` | 共用(pid + `TEMP_SEQUENCE`) | 同左 |

`replace_existing` 按 plan 要求进签名(unix 没有可移植的"不替换"rename,所以它靠忽略
参数来回答这个问题)。碰名重试从 `is_name_collision(&anyhow::Error)` 一处判定,两边都
把平台错误保成 `io::Error` 再上抛。

**windows 顺带拿到了两样它本来没有的**:

1. **进循环前的 `verify_parent_binding`**。unix 版一直有,windows 版只有循环里那一次。
2. **`CommitFault::ReplaceTempName`**。gate 从 `#[cfg(all(test, unix))]` 改成
   `#[cfg(test)]`,`replaced_temporary_name_never_reaches_target` 上的 `#[cfg(unix)]`
   一并去掉——按本批纪律(b),统一之后两个平台拿同一套测试钩子。

注入实现换了形状:原来是 `std::fs::remove_file` + `std::fs::write`(**按路径**),现在是
`rebind_temp_name()`——另建一个 temp、写进攻击者字节、`commit_rename` 盖到原名上。理由
是 windows 删不掉一个还开着句柄的名字(unlink + create 那条路走不通),而 rename 两个
平台都做得到;顺带 unix 那半也从"按路径"变成了"相对 parent fd",与这个文件里其余所有
操作一致。被盖上去的那个 decoy 句柄**留到失败清理**(`decoy` 局部活过内层闭包),让
失败路径在两个平台上都能把两个文件都收掉——否则 unix 按名字删掉的是 decoy、windows 按
句柄删掉的是原件,各自漏一个。

### 三、验收(各自单独跑,当场取退出码)

- `crates/provider/tests/{anthropic,openai,responses,effort_probe}.rs` **一个字符没改**,
  全绿——这是"骨架搬家没改语义"的唯一证据;
- `fs.rs` 的 mutation 测试除了去掉一个 `#[cfg(unix)]` 之外没改,全绿;
- `grep -c 'SseParser::default()'` 在 provider 生产代码里从 3 变 1(`stream.rs`),
  `GuardedBody::new` 同样从 3 变 1;
- `cargo fmt --all --check` / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo test --workspace` 三条分别跑,退出码 0。

另外做了一次**反向验证**(教训 126):把 `verify_temp_binding` 那一行停掉再跑
`replaced_temporary_name_never_reaches_target`,它当场变红(攻击者字节落到了 target 上,
被后面的 "changed immediately after commit" 撞出来)——证明新的注入真的把那场竞态摆出来
了,而不是靠别的检查顺手挡住。

**行数**:plan 估的是 SSE −60 / fs −70,实际是 SSE 净 +31(三条 rail −27,`SseFrames`
+58,其中约 20 行是文档)、fs 净 +2。plan 的估算按"抽出去的是一个 20 行自由函数"算,
而实际抽出去的是一个带文档的类型;真正的收益不在行数,在**那五道守卫从三份变一份**、
**那三个 `verify_*` 的调用顺序从两份变一份**。

### 四、没能验证的一处

本机只装了 `aarch64-apple-darwin`,windows 分支既跑不了也**编译不了**(`rustup target
list --installed` 只有一项)。所以上面"windows 拿到的两样"是按已有 helper 拼的(没有新
写一行 NT API),但没有编译器背书。真要上 windows 时,第一件事是跑
`replaced_temporary_name_never_reaches_target`——它现在会在那边跑起来了。
