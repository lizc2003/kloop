# Plan 234 ✅ — DeepSeek 搜索后端

> 2026-10-19。用户要求新增 DeepSeek search provider。
> 状态：已实现并完成本地门禁与真实测试；无待询问项。

## 范围与契约

- `[web].search_provider = "deepseek"` 使用同节 `api_key`；默认仍为 Tavily，
  不自动改用户配置，不增加环境变量、模型调用、后端切换或远程 fetch。
- 新 `DeepSeek` 实现 `SearchBackend`，Bearer 鉴权调用
  `POST https://api.deepseek.com/v1/tools/search`，显式发送 `query`、固定五条的
  `count` 和 `need_content: false`。不透传其他后端字段，避免未知字段被静默忽略。
- 规范化后的允许域名转 `site:`，多个域名各带完整查询并用 OR 连接；排除域名
  转 `-site:`，日期转 `after:` / `before:`，精确短语保留原查询引号。
  日期及站点约束放入每个 OR 分支。日期沿用服务端 before/after 语义，不声称
  本地证明了日期和短语的严格匹配；工具说明提示日期/短语需复核，HTTP(S)/域名
  本地复核继续保留。
- 最终查询连同拼接操作符必须在 128 个 Unicode 字符以内；超限明确报错，
  不截断用户查询或丢弃过滤条件；后端内部条数限定 1–50。
- 读取 `results`、`title`、`url`、面向模型的 `summary` 和可选 `date`，继续使用
  严格响应类型检查、真实空数组/本地筛空区分、5 MiB 下载和 50k 输出上限。
  不请求网页正文，不把网页短摘要与长摘要拼接重复。
- CLI 接线、配置示例、未知 provider 提示、工具说明与 DESIGN 同步。工具 schema 沿用
  plan 233，无新字段。

## 验证

- HTTP mock 覆盖鉴权、路径和完整请求体；单/多域名、排除域名、双/单边日期、
  精确短语、摘要与可选日期、本地复核、缺失/错型结果、HTTP/JSON 错误。
- 输入限制在请求前拒绝：128/129 字符边界（按 Unicode 计数）、操作符拼接后
  超限、非法后端条数。CLI 测试确认配置能注册且无 key 时仍只注册 fetch。
- 沿用会话已授权的 DeepSeek key 做真实请求，直接调用生产 `WebTools::search`，
  记录样本及限制，不保存凭据到仓库，不改当前 Tavily 配置。
- 外层执行沙箱之外 `make check` 全绿；完整差异和未暂存/暂存 diff 检查通过，
  一次英文 `feat(plan234): ...` 提交。完成后补 ✅、交接和新教训。

## 官方依据

- [网页搜索与抓取](https://api-docs.deepseek.com/zh-cn/guides/search/)
- [Search API](https://api-docs.deepseek.com/api/search/)

## 完成记录

- 已接入 `deepseek` 配置、适配器、工具说明与长摘要/可选日期输出；默认和用户配置
  未改，复用现有网络、响应、输出上限与本地 URL/域名复核。
- 先扩展注册回归，旧实现因 DeepSeek 只注册 fetch 而失败；新增八条 HTTP mock
  回归全部通过，覆盖完整请求体、鉴权、约束、响应错型及 Unicode/条数边界。
  CLI 注册测试也已在首轮完整门禁通过。
- 用会话已授权的 key，经隐藏进程输入调用生产实现，八组真实请求全部成功，
  产生 38 个候选（含重复），30 条带日期。单站点允许/排除的服务端候选均合规。
  日期区间返回三条、起始/结束单边各五条；日期字段有 UTC 跨日前移，不能只按
  日历日字符串判定服务端是否按网站日期执行，也不修改原日期来伪造一致。
- 已知精确短语和不存在的随机短语都返回五条，部分候选不匹配；八次 HTTP 成功
  不代表短语严格匹配。实测后在该后端工具说明追加日期/短语需复核，并同步 DESIGN、
  能力报告与 HANDOFF 教训 243。未增加自动重试、后端切换或模型调用。
- 临时探针和运行结果仅在临时目录，密钥仅在进程内存，不进入探针、输出、用户配置
  或提交。真实测试没有验证多域名 OR 的引擎召回，相关参数组合由 mock 覆盖；
  不据此声称整体搜索质量改善。
- 最终文件在外层执行沙箱之外的完整 `make check` 全绿，包含 fmt、clippy、workspace
  debug 全量测试和本机 parity；Web 单测 32 条及搜索契约 9+8 条均通过，原有
  20 MiB fetch 传输测试仍按原设置忽略。实测后的工具说明和补齐后的配置示例均
  已纳入完整复核；CLI 154、core 1127、plain/TUI PTY 7/20、server 协议 40、
  TUI 275 条测试通过。
- 完整差异已核对，`git diff --check` 与 `git diff --cached --check` 通过。实现提交
  为本文件所在的 `feat(plan234): add DeepSeek search provider` 提交
  （`git log -- docs/plan/234-deepseek-search-provider.md` 可查提交号）。
