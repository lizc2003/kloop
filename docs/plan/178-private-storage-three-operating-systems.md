# Plan 178 ✅ — 私有存储:一个文件里住着三套系统调用

> 已完成 2026-09-22,提交 `03e891f`。落地读数见文末「五、结果」。

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。

## 一、现状

`rust/crates/cli/src/private_store.rs`,**1088 code 行 / 1164 总行**。
它是仓库里 cfg 密度最高的文件:

```
25 × #[cfg(unix)]        24 × #[cfg(windows)]        15 × #[cfg(not(any(unix, windows)))]
```

同一个概念被写了三遍,平铺在一个文件里:`read_private_string_impl` 三份(27/38/49)、
`write_private_atomic_impl` 三份(102/114/131)、`private_dir_lookup_flags` 三份
(149/154/162)、`open_or_create_child_directory`、`open_parent_directory`……
Windows 那一套还另起了 `windows_*` 前缀的 **20 个函数**(474–878),
它们其实就是 windows 分支的私有实现,只是没有地方放。

**对外面只有四个符号**:`read_private_string`、`write_private_atomic`、
`PrivateDir`、`ExclusiveFileLock`。1088 行里真正跨平台的契约就这么点。

## 二、切法

facade 留在 `private_store.rs`,三套实现各自成文件:

| 新文件 | 内容 | 预估 |
|---|---|---|
| `private_store/unix.rs` | 描述符相对的 openat/no-follow 一套,`#[cfg(unix)]` 整体条件编译 | ≈400 |
| `private_store/windows.rs` | 现在那 20 个 `windows_*` 函数,去掉前缀 | ≈420 |
| `private_store/fallback.rs` | `not(any(unix, windows))` 那 15 处 | ≈120 |
| `private_store.rs`(留) | 四个对外符号 + 三个平台共用的校验(`validate_component`、`validate_private_file`)+ `mod` 声明 | ≈150 |

**关键是先把"平台契约"写出来再搬**:三份实现必须有同一个签名表(就是现在那些
`*_impl` 的签名),搬完之后 facade 里应该**一个 `#[cfg]` 都不剩**——全部收进
`mod` 声明那三行。这是判断这次拆对没拆对的硬标准。

## 三、坑

- **不要为了消 cfg 造一个 trait**。三套实现的生命期与错误类型不一样(unix 拿
  `std::fs::File` 当目录描述符,windows 走 `HANDLE` 包装),抽 trait 会逼出一层
  `Box<dyn>` 或一堆关联类型,而调用方只有四个符号——**同名函数 + `#[cfg]` mod 就够了**,
  这也是 `core/src/tools/fs.rs` 已经在用的形状(plan 179)。
- **`windows_*` 前缀去掉之后会和 unix 那边重名**,这正是要的效果;但搬的时候必须一次一个
  函数地对照签名,现在三份的参数顺序并不完全一致(`open_private_dir_at_with_flags` 与
  `open_windows_relative` 就不是一个形状)——**统一签名是这次拆的一部分,不是副产品**。
- 文件里有一条 `#[cfg(test)]`,测试引用了 `tests::abrupt_process_exit_releases_descriptor_lock`
  (被 `cli/tests/` 里的某条按名字提到)。搬代码前先 grep 这个名字,确认它留在能被找到的地方。
- **这个文件是安全边界**(0600/0700、no-follow、reparse 拒绝)。重构提交里**一行语义都不能改**;
  任何"顺手收紧"都单独立一次提交,并且要带测试。

## 四、验收

- `make check` 全绿。macOS 上跑得到的是 unix 分支;**windows 分支只能靠 CI**,
  所以这条 plan 的提交必须等 CI 三平台绿了才算完(本批唯一有这个要求的两条之一,另一条是 179)。
- facade 里 `#[cfg]` 归零。
- `private_store.rs` 降到 ≈150。

## 五、结果(2026-09-22,提交 `03e891f`)

`private_store.rs` **1291 code → 78 code + 200 行测试**,三套实现各自成文件:
`unix.rs` 342 / `windows.rs` 485 / `fallback.rs` 184。开工时文件已经比 plan 里
记的 1088 涨到 1291(总行 1164 → 1398),切法不受影响。

facade 里 `#[cfg]` 归零达成,只剩 `mod platform` 那三处声明——用
`#[cfg(...)] #[path = "private_store/<os>.rs"] mod platform;`,和 `tools/fs.rs`
的 `#[path]` 同形。为此多了两件 plan 没预见的事,都是"最后一个 cfg"逼出来的:

- **`PrivateDir` 成了 `platform::Dir` 的 newtype**。它的字段本身是 cfg 的
  (`file: File` / `path: PathBuf`),留在 facade 就留着 cfg。
- **`validate_private_file` 里那段 `#[cfg(unix)]` 权限检查变成契约的一员**
  `validate_private_permissions(&Metadata, &str)`,unix 查 0o077,另两家 `Ok(())`
  并各带一句"为什么没得查"。共用的那半截(必须是普通文件)还在 facade。
- 同理 `private_file_name` 在 `unix.rs`/`windows.rs` 各留一份:一个
  `cfg(any(unix, windows))` 的 helper 在"不带 cfg 的 facade"里没有位置。

**可见性一处都没提**:`validate_component` 等三个共用项在 facade 里仍是私有 `fn`——
Rust 的私有项对后代模块可见,子模块 `use super::x` 直接够得到。反方向(facade 用
platform 的东西)才需要 `pub(super)`。

平台契约表(三个文件签名逐字相同):`read_private_string`、`write_private_atomic`、
`validate_private_permissions`、`Dir{open,ensure,read_string,write_atomic,open_lock_file}`、
`lock_file`、`unlock_file`。

验证:`make check` + `make mock` 全绿。**windows/fallback 分支在本机也核过了**——
整 crate 交叉编译走不通(`ring` 的 C 构建没有 windows target),但搭一个只含这四个
文件的 scratch crate 就能 `cargo check/clippy --target x86_64-pc-windows-msvc` 和
`--target wasm32-unknown-unknown`(既非 unix 也非 windows,正好走 fallback),两边
零 error 零 warning;往两个文件里各塞一个故意的类型错误,确认这套 harness 先红。
CI 三平台绿仍是这条的收工条件。
