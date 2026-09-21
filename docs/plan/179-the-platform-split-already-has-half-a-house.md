# Plan 179 — 文件工具:平台分支已经盖好半间房子了

> 本批判据与统一纪律见 `HANDOFF.md` 第〇节。**建议紧跟 178 做**,同一个形状做第二遍。

## 一、现状

`rust/crates/core/src/tools/fs.rs`,**1551 code 行 / 3416 总行**。
cfg 分布:`24 × unix`、`15 × windows`、`8 × not(any(unix, windows))`、`2 × not(windows)`。

**半间房子已经盖好了**:文件顶部 16–18 行就是

```rust
#[cfg(windows)]
#[path = "fs/windows.rs"]
mod windows;
```

`fs/windows.rs` 已经存在(637 行),装的是 Win32 的底层帮手。但**平台三胞胎本身还留在
fs.rs 里**:`open_read_target` 三份(257/285/296)、`open_or_create_child_directory` 三份
(1024/1077/1103)、`open_parent_directory` 三份(1113/1127/1133)、
`has_multiple_hard_links` 三份、`file_identity` 三份、`create_temp` / `commit_rename` /
`discard_temp` / `atomic_replace` / `sync_parent` / `cleanup_created_directories` 各两到三份。

plan 139 做过 `atomic_replace` 的三平台钩子,那次只收了一个函数;**这次把同一个形状推完**。

## 二、切法

沿用已有的 `#[path = "fs/..."]` 形状,把 `fs/windows.rs` 从"Win32 帮手"升级成
"windows 平台实现",另起两个兄弟:

| 新/改文件 | 内容 | 预估 |
|---|---|---|
| `fs/unix.rs`(新) | 24 处 `#[cfg(unix)]` 的实现 | ≈350 |
| `fs/windows.rs`(改) | 已有内容 + 15 处 `#[cfg(windows)]` 的实现 | ≈600 |
| `fs/fallback.rs`(新) | 8 处 `not(any(unix, windows))` | ≈100 |
| `fs.rs`(留) | 工具语义:`read_file` / `write_file` / `edit_file` 的输入解析、`Mutation` / `CommitTarget` / `ReadRequirement`、分页与行号、重读 advisory、编辑资格 | ≈700 |

和 178 同一条硬标准:**搬完 fs.rs 里只剩 `mod` 声明那几行带 `#[cfg]`**。

## 三、坑

- **`fs/windows.rs` 改完可能顶到 800**(现在 637 总行,再进 15 处实现)。它**不在基线里**,
  所以一旦超过 800 就是门禁失败,而不是加一行基线。**不用事先量**——门禁只在超标时报数,
  搬完跑一次 `make check` 就知道:过了就是没超,失败信息里会给准数。
  真超了就再分一层(比如 `fs/windows/{spawn,handle}.rs`),**不要去动基线**。
- `ReadRequirement`(1353–1380)与 `unread_edit_hint`(1396)是 plan 155/156 的产物,
  是**工具语义不是平台实现**,留在 fs.rs。
- `#[cfg(test)]` 在这个文件里有 7 处,其中一处是 `#[cfg(all(test, windows))]`。
  平台相关的测试跟着平台实现走,跨平台的留在 fs.rs。
- 这个文件同样是安全边界(no-follow、硬链接拒绝、原子替换)。**重构不改语义**。

## 四、验收

- `make check` 全绿 + **CI 三平台绿**(windows 分支本机跑不到)。
- fs.rs 里 `#[cfg]` 只剩 mod 声明。
- 三个平台文件都 ≤800 code 行;fs.rs 降到 ≈700 后跑 `make arch-baseline`
  ——它会从基线里被摘掉。
