# Plan 40 — 全局 provider 配置（`~/.kloop/config.toml`）

> 2026-07-24 开工。承接 plan 39 的日常 TUI dogfooding：provider 配置应由 kloop 原生读取，不再依赖 `~/.kloop/env.local` + wrapper source。

## 目标

1. `~/.kloop/config.toml` 保存进程级 provider/profile、model、base URL 与认证；TUI/plain/headless/app-server 共用一次解析结果。
2. cwd 下 `.kloop/config.toml` 继续只承载 permissions/MCP/web/hooks/sandbox/agents/codemode，绝不混入凭据，也不影响 provider。
3. 兼容现有 provider env 作临时覆盖/CI 入口，但日常启动不需要 source env 文件。
4. 凭据不进入 model/config-read/log/rollout/子 shell；模型的 read_file/grep/glob/bash 不能读取敏感配置。

## 全局配置 schema

采用 Codex 风格子集，并用 `wire_api` 明确 kloop 的三条 adapter：

```toml
model = "gpt-5.6-sol"
model_provider = "gw_router"
model_reasoning_effort = "xhigh"

[model_providers.gw_router]
name = "gateway" # 可选展示名
wire_api = "responses" # responses | chat | anthropic
base_url = "https://example/v1"
http_headers = { Authorization = "Bearer ..." }
model = "gpt-5.6-sol" # 可选 profile 默认
```

- 自定义 profile 必须写 `wire_api`；内建名 `anthropic`、`openai`/`openai-compat`、`openai-responses` 可按名推断。
- `anthropic` 只收 `x-api-key`；`chat`/`responses` 只收 Bearer Authorization，不做任意 header 透传。
- Anthropic profile 可选 `cache`、`thinking = "off" | "adaptive" | <budget>`；Responses profile 可选 `effort`，否则读顶层 `model_reasoning_effort`。
- URL 只收绝对 http(s) base；拒 userinfo/query/fragment 和已带终端 endpoint 的 URL；去掉尾 `/` 后由 adapter 继续拼 endpoint。
- 未知键、错误类型、空 key/model/base、错误 header/wire 均 fail closed，错误不回显 secret/TOML 源行。

## 优先级

1. `--mock` hermetic，不读 HOME/config/provider env。
2. provider：`KLOOP_PROVIDER` > `model_provider` > 旧 key 自动探测。
3. key/base：provider env > profile > 既有默认 base。
4. model：provider-specific env > `KLOOP_MODEL` > profile `model` > 顶层 `model` > Anthropic 默认；OpenAI 缺最终 model 报错。
5. `KLOOP_CACHE`/`KLOOP_THINKING`/`KLOOP_EFFORT` > profile/top-level > 既有默认。

显式选中的 provider/profile 配不完整时不得 fallback 到别轨。

## 实现切片

### A. resolver + 入口收敛

- 新增 `crates/cli/src/provider_config.rs`：typed parse、文件 mode/symlink 检查、env-map resolver、`ResolvedProviderSettings`。
- `main.rs` 早退后解析一次 `Arc<ResolvedProviderSettings>`；TUI/plain/headless/server factory/model-list/config-read 共用。
- `startup.rs::config_from_env` 改吃 resolved settings；`server_model_info` 不再自行读 env。
- `PERMISSIONS_CONFIG` 重命名 `PROJECT_CONFIG`；项目 config 出现 provider 顶层键时明确拒绝。

### B. credential boundary

- global config 要求 Unix `0600`、拒 symlink；目录迁移成 `0700`。
- `read_file` 对敏感路径直接拒；grep/glob 保持读取前过滤。
- Bash 在 sandbox/read-only/bypass 之前检查 argv 的词法 + canonical path，挡 `.kloop`/`.env*`/`.ssh` 等及 symlink/case alias；不能分析的 shell 不借 sandbox auto-allow。
- Bash 子进程移除 provider/search key env；macOS seatbelt 对 global config 加 file-read deny。
- provider 非 2xx 错误中的已知 key 必须脱敏并限制正文。

### C. 文档与迁移

- README/help/HANDOFF 区分 global provider config 与 project policy config。
- 原子生成 `~/.kloop/config.toml`（0600），wrapper 不再 source env.local；清 env 后做真实 headless/TUI/app-server 验证，再删除 global env.local。
- repo root `.kloop/env.local` 仅保留 Anthropic/Tavily 专项验收，并改 0600。

## 完成标准

- resolver/strictness/precedence/secret-redaction/server-consistency/read-path/bash-env/sandbox 测试齐全。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test`、`cargo run -p kloop -- --mock` 全绿。
- 无 provider env 的真实 headless 与 TUI 走 `~/.kloop/config.toml` 成功；app-server `model/list`/`config/read` 正确且无 key/header/base。
- 实测 `read_file ~/.kloop/config.toml`、bash `cat`/`base64`/symlink 均在模型拿到内容前被拒。
- 行为/docs/本 plan/HANDOFF 更新后一次 commit；本机配置、launcher、凭据不进 git。

## 完成记录 ✅（2026-07-24）

- 新增 `provider_config.rs`：Codex 风格 profile、三 wire、strict schema/URL/header、env 覆盖优先级、0600/非 symlink、project provider-key 拒绝；secret-bearing resolved settings 不实现 Debug/Serialize。
- `main.rs` 启动解析一次并贯通 TUI/plain/headless/app-server/model-list/config-read；thread model override 只改 model。
- 凭据边界补齐：read_file 敏感读硬拒、grep/glob 复用过滤、Bash raw/argv/canonical/symlink/recursive hidden root 检查、子 shell provider/search env scrub、macOS seatbelt global config read deny、provider HTTP 错误 key 脱敏/4KiB 上界。
- 文档/help/HANDOFF 已区分 global provider 与 project policy。
- 本机迁移：`~/.kloop/config.toml` 已原子创建为 0600、`~/.kloop` 为 0700；launcher 不再 source env.local；global env.local 已删除，repo 专项 env.local 改 0600。
- 验证：fmt、clippy `-D warnings`、全 workspace tests 全绿；clean-env headless 回 `GLOBAL_CONFIG_OK`；app-server `model/list`/`config/read` 回 `openaiResponses / gpt-5.6-sol` 且无 key/base；clean-env 真 TUI 回 `NATIVE_CONFIG` 并干净退出；真实模型读取 fake `.kloop/secret.txt` 得 `BLOCKED`，sentinel 未外泄；真实 seatbelt symlink read-deny 测试通过。
- 提交：本次（plan 40，见 git log）。
