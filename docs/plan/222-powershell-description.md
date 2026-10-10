# Plan 222 — PowerShell 也带一个 description（显示专用）

> 状态：✅ 已完成（提交 SHA 以本条所在提交为准）
>
> 依赖：Plan 62（Native Windows PowerShell）、Plan 156（bash 的 display-only `description`）、Plan 221（把标签与命令拆开）。

## 起因

用户问「PowerShell 有没有 description」，得到否定答案后要求也加上：前台 PowerShell
的转录行只有 `● PowerShell` / `PS> <脚本>`，长脚本没有一句人类可读的说明，而 bash 早就有
这个字段（plan 156）。PowerShell 只前台、不会成为 `BackgroundTask`，所以这次只补工具行本身，
不牵后台生命周期。

## 裁决与实现

1. `core/src/tools/builtin.rs` 的 `powershell_def()`：schema 增 `description`，与 bash 同契约
   同措辞——`type: ["string","null"]`、`minLength: 1`、`maxLength: MAX_DISPLAY_DESCRIPTION_CHARS`、
   `pattern: ".*\\S.*"`，prose 说明它只领用户转录行、不进 PowerShell、不改结果，并要求每次调用
   都给一个。tool 自己的 doc 字符串不动（`shell_catalog_follows_the_frozen_availability_snapshot`
   断言它含 “background execution and Windows shell sandboxing are unavailable”）。
2. `core/src/tools/powershell.rs` 的 `validate_input`：白名单加 `"description"`，并调用
   `super::optional_display_description(input, "powershell")?` 做同一套严格校验（非空、单行、
   无控制字符、≤200 Unicode 字符），在 spawn 之前失败。
3. `tui/src/toolrow.rs`：`"powershell" => ("PowerShell".into(), s("description"))`。布局仍与
   Bash 同形——表头 `● PowerShell <描述>`，命令仍单起一行 `PS> <命令>`。
4. `rust/DESIGN.md` 的 Native Windows PowerShell 一节：`powershell {command, description?, timeout_ms?}`，
   并补一句它与 bash 同契约、只领 TUI 行。

## 非目标

- 不动 powershell 的 tool doc、权限模型（仍 `PowerShellOpaque`、每次询问）与前台-only 定位。
- 不声明模型会不会开始给 description：schema 措辞是 model-facing，效果要真实采样才算数，
  本次不断言。

## 验证

- `make check`（fmt + clippy + workspace test + release 测试 + parity）全绿。
- 断言：`description_is_display_only_and_carries_the_bash_contract`（接受合法值；空白 / 多行 /
  控制字符 / 超长 / 非字符串被拒）、`powershell_row_shows_the_description_above_the_script`
  （`✓ PowerShell Check the path` + `  PS> Get-Location`）；原有
  `powershell_row_keeps_the_original_script` 与 `input_rejects_every_non_v1_field` 保持不动。