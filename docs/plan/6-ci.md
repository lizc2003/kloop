# Plan 6 — CI

> 一个会话完成。开工前先读根目录 HANDOFF.md。

## 目标

GitHub Actions 流水线:每次 push/PR 跑 fmt 检查、clippy(-D warnings)、全量测试。这是测试基建的收尾——54 个测试只有被强制执行才算数。

## 范围

- 做:`.github/workflows/ci.yml`;修掉 clippy -D warnings 暴露的所有既有警告(当前从未跑过 clippy,预计有一批)。
- 不做:发布/打包、多平台矩阵(macOS 一个就够,团队都是 mac;Linux 可以顺手加,不强求)、覆盖率统计。

## 实施

1. 先本地跑 `cargo clippy --workspace --all-targets -- -D warnings`,清干净。
2. workflow:checkout → rust toolchain(stable,带 rustfmt/clippy)→ cache(Swatinem/rust-cache)→ `cargo fmt --check` → clippy → `cargo test --workspace`。工作目录 `kloop/`(注意仓库根不是 workspace 根)。
3. 若仓库还没推远端:workflow 文件照写,验证方式改为本地逐条执行同样命令。

## 完成标准

- 本地三连(fmt --check / clippy -D warnings / test)全绿。
- ci.yml 提交,若有远端则实际跑绿一次。
- README 加 CI 说明一行。
