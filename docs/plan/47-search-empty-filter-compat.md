# Plan 47 — grep 空过滤器兼容

## 背景

部分调用方会为 grep 的可选过滤参数发送 `glob: ""` 或 `type: ""`。kloop
当前将两者保留成实际过滤值，导致空 glob 进入 override、空 type 报
`unknown file type ''`。

## 范围与决定

- 在 `GrepArgs::parse` 输入边界将精确零长度的 `glob` / `type` 规范化为
  `None`，等同未传。
- 不做 `trim`；纯空白字符串仍保留为调用方显式给出的实际过滤值。
- `build_walk`、工具 schema、provider/dispatch 路径保持不变。
- 本次不做 `context` 别名、`-o`、glob 按空白/逗号拆分、长行策略调整，
  也不改变独立 `glob` 工具的必填 `pattern`。

## 测试

- 解析层锁定 `"" -> None`，同时锁定 `" " -> Some(" ")`。
- 行为层证明空 glob 与空 type 均和省略过滤器返回相同结果。
- 现有非空 glob、合法 type、未知 type 测试继续覆盖原行为。

## 完成标准

- README 与 HANDOFF 同步。
- `cargo test -p kloop-core tools::search::tests`
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo run -p kloop -- --mock`
- 一次提交。

## 完成记录 ✅（2026-07-27）

- `GrepArgs::parse` 已将精确空的 `glob` / `type` 收敛为 `None`；纯空白字符串仍
  原样保留。
- 解析层测试锁定零长度与空白的边界，行为层测试证明两个空过滤器均等同省略；
  现有非空过滤与未知 type 测试保持通过。
- README 与 HANDOFF 已同步；context/-o、glob 拆分、长行策略及独立 glob 工具
  均未改动。
- 验证：`cargo test -p kloop-core tools::search::tests`、`cargo fmt --all --check`、
  `cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、
  `cargo run -p kloop -- --mock`、`git diff --check` 全绿。
- 提交：本次（plan 47，见 git log）。
