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
- fs.rs 降到 ≈700
  ——它会从基线里被摘掉。

## 五、✅ 完成(2026-09-23)

`make check` 全绿 + **windows 交叉 check 绿(scratch crate,含测试目标)**。

### 两处 plan 已经过期,开工时核实过

- **"800 门禁 / 从基线里摘掉"这条不存在了。** 文件体积棘轮(`architecture-policy.toml` +
  `architecture-baseline.toml` + `crates/cli/tests/architecture.rs`)已于 2026-09-21 整体删除
  (见教训 172 后记),`fs/windows.rs` 涨到 747 行不再有任何东西会报。于是 §三 第一条坑与 §四
  第三条验收作废,**这次没有任何机器在量行数**。
- **"CI 三平台绿"也不存在了**(2026-09-22 删掉 CI)。windows 的唯一证据是下面那个 scratch
  crate,而**编过不等于跑过**——照实说:`fs/windows.rs` 在 `x86_64-pc-windows-msvc` 上类型检查
  通过,**未在 Windows 上执行过一行**。

### 与 §二 切法的三处差异

1. **模块名是 `platform`,不是三个不同的名字。** 三个 `#[cfg]` + `#[path]` 指向
   `fs/{unix,windows,fallback}.rs`,都叫 `mod platform`,后面一个 `use platform::*;`。于是
   fs.rs 里**平台 cfg 精确地只有那三行**(其余 `#[cfg(test)]` 按 §三 留下)。
2. **少了一个模块:`atomic_replace` / `is_name_collision` / `rebind_temp_name` 三个都没搬,
   而是去掉了 cfg 留在 fs.rs。** 它们原来带 `#[cfg(any(unix, windows))]`,但读进去会发现**它们
   跟平台无关**——只调 `create_temp`/`commit_rename`/`temp_name`,以及 downcast 一个
   `io::Error`。带 cfg 的唯一原因是 fallback 上没有那几个原语。于是让 `fs/fallback.rs` 提供会
   bail 的 `create_temp`/`commit_rename`/`discard_temp`(各自带上原来整个操作refuse 用的那句
   `safe file mutation is unsupported on this platform`),三处 cfg 与那份 fallback
   `atomic_replace` 一起消失。**少一个模块、少三处 cfg,算法留在语义那一侧。**
   代价写清:fallback 平台上的错误从裸那一句变成被 `cannot create temporary file for X:` 包一层
   ——那个平台既没有用户也没有测试,而 §三 的"重构不改语义"指的是可观测行为,不是错误文案的
   包裹层数。
3. **多提了一个接口函数 `reject_unsafe_component`。** `validate_component_name` 里内嵌着一个
   `#[cfg(windows)]` 块(Unicode、分隔符/盘符、结尾点或空格),它是平台规则而不是共享校验,
   留着就等于 §二 那条硬标准不成立。三个平台各一份,unix/fallback 直接 `Ok(())`。

### 撞名与去重

`fs/windows.rs` 里已有的四个底层帮手和搬进来的包装器同名。两个**包装器是纯转发**
(`has_multiple_hard_links`、`file_identity`),直接删掉包装器、让已有的那份顶上;另两个签名不同,
把**底层那份**改名让路:`open_or_create_child_directory` → `create_or_open_directory_handle`、
`remove_created_directory` → `remove_directory_by_identity`。

### 这次的教训(已进 HANDOFF 教训 183)

`cargo build` 在 macOS 上**一次就过**,而那时 `fs/windows.rs` 里有 7 处 `windows::` 前缀
指向一个已经不存在的模块路径——**因为那个文件在本机一行都不编译**。整 crate 交叉编译走不通
(`ring` 的 C 部分要 MSVC),所以证据只能来自 scratch crate:它一次报出 8 个错(缺
`use super::*` 与 `use std::path::Path`)。**搬平台代码时,"本机编过"是零信息。**
