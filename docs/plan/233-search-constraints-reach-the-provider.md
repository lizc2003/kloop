# Plan 233 ✅ — 搜索条件传到服务端，结果保留日期

> 2026-10-19。用户要求把参考项目调研中的首批优化写成计划并实现。
> 状态：已实现并完成验证；接口选择已根据当前会话确定，无待询问项。

## 问题与范围

当前后端只收到 query 和固定条数，域名条件在前五条里筛选，可能漏掉目标站点；
日期和精确匹配不能表达，来源日期被丢弃；合法 JSON 中缺失/错型的结果数组被当成
空结果。首批只修这四处，不改变配置、默认后端、固定五条、basic 深度和权限管线。
语言参数、DeepSeek 新后端、条数调整、多查询、缓存和自动切换留待后续。

## 契约与落地

1. `web_search` 保留 `query`、互斥的 `allowed_domains` / `blocked_domains`，增加
   `start_date?`、`end_date?`（真实的 `YYYY-MM-DD` 日历日期）和 `exact_match?`
   （默认 false）。校验空白 query、日期格式/顺序，精确匹配要求至少一对双引号内
   有非空短语，且拒绝未配对引号；
   输入错误在请求前拒绝。域名沿用 URL/通配前缀的兼容规范化，非法项明确报错，
   不再悄悄丢弃。后端收到完整、规范化的请求和内部固定条数。
2. Tavily 域名映射为 `include_domains` / `exclude_domains`，允许列表显式
   `include_domains_mode: restrict`；日期映射为同名字段，并在给出日期时启用
   `filter_by_published_date`，剔除日期未知的来源；精确匹配发送 `exact_match`。
   始终请求 `include_published_date`，保持 basic，不请求回答或完整正文。
3. Brave 域名通过文档列出的 `site:` / `OR` / `NOT site:` 表达；有多个允许域名时
   为每个域名重复 query，避免 OR 让后续站点失去关键词条件。日期使用 `freshness`
   的自定义区间，必须同时提供起止日期，单边日期在请求前说明该适配器限制。
   精确短语直接使用 query 中已有的引号。操作符属服务端实验能力，输出说明域名仍
   经过本地复核，不把发出参数等同于证明搜索引擎严格兑现所有条件。
4. 继续在本地按 HTTP(S) URL 和精确 host/子域边界复核。服务端无结果保持
   `No results found`；非空候选被复核全部移除时，明确说明候选被过滤，不声称全网
   没有匹配。非法 URL 的移除也归入这类本地筛选。
5. `SearchHit` 增加可选日期，分别读取 Tavily `published_date` 和 Brave `page_age`。
   输出标为服务端估计的发布/更新时间，缺失或 null 不编造。结果信封/条目错型、
   结果数组缺失是后端响应错误，不伪装为无结果。搜索响应独立限 5 MiB，输出仍限
   50k 字符。实施复核发现原代码共用 fetch 的 20 MiB 常量，错误文案与设计仍写
   5 MiB；本批分开常量并以 5 MiB + 1 的真实响应验证，避免后续相互牵连。

## 参考与官方契约

- Codex：结构化过滤进入托管搜索请求；grok-build：配置/请求域名约束进入服务端。
- CodeWhale：完整请求和条件执行状态的分层；其 Tavily 域名仍后筛，只借判据。
- DeepSeek Harness：可选来源日期保真；本批不引入其多查询或模型辅助搜索。
- [Tavily Search](https://docs.tavily.com/documentation/api-reference/endpoint/search)
- [Brave 参数](https://api-dashboard.search.brave.com/api-reference/web/search/get)
- [Brave 操作符](https://api-dashboard.search.brave.com/documentation/resources/search-operators)

## 验证与完成标准

- 本地 HTTP mock 断言真实请求体/查询参数，而非只测组装函数：两类域名、日期、
  精确匹配、日期返回、未命中的 mock 请求数和本地复核边界。
- 覆盖非法日期/逆序/空白/无精确短语/非法域名的零请求、单边 Brave 日期明确拒绝。
- 覆盖真实空数组、缺失/错型数组和条目、HTTP/JSON/超时/体积边界；原 fetch 回归保留。
- core 工具 schema 与 CLI ToolSource 接线测试同步，保留严格未知字段和互斥列表。
- 改写 DESIGN 现有 Web search 段落；HANDOFF 补交接和教训。
- 外层执行沙箱之外 `make check` 全绿；`git diff --check`、暂存后
  `git diff --cached --check`；一次英文 `feat(plan233): ...` 提交。
- 不以 mock 证明真实引擎召回率改善；实现门禁不使用凭据，不改用户配置或参考语料。
  用户随后要求的真实 API 验证另记，凭据仅运行时读取，不进入仓库或输出。

## 完成记录

- 后端现在接收完整 `SearchRequest`；工具 schema 与 CLI 接线同步新增日期和精确
  匹配，规范化后的域名在服务端召回前生效，本地 HTTP(S)/host 边界复核继续保留。
- Tavily 下传 include/exclude、restrict、日期/日期未知过滤和 exact_match，始终请求
  来源日期；Brave 下传各站点完整 query 的 OR、NOT site、双边 freshness 和原有
  精确短语，单边日期明确拒绝。默认后端、basic、固定五条与权限管线未变。
- 来源日期按服务端估计的发布/更新时间输出；真实空数组、本地候选全部被移除、
  缺失/错型结果数组及条目分开处理。搜索下载常量独立为 5 MiB，错误上限由常量
  生成，原传输边界回归改用 5 MiB + 1；fetch 仍为 20 MiB。
- 新增九条 HTTP mock 契约回归；其中先写的三条在旧实现全部失败：新字段被拒绝、
  include_domains 缺失、畸形成功响应被当成空结果。实现后 web 原有 32 条及新增
  9 条全部通过，原有 20 MiB fetch 传输测试仍按原设置忽略。
- 最终代码在外层执行沙箱之外的完整 `make check` 全绿：fmt、clippy、workspace debug
  测试和本机 parity；core 1127、CLI 154、plain/TUI PTY 7/20、server 单测/协议
  26/40、TUI 275 条测试通过。首轮已通过，下载边界补改后的最终版本再次完整通过。
- DESIGN 原 Web search 段已改写；HANDOFF 补教训 242。实现及本地门禁阶段没有
  使用真实 API、凭据或模型样本，不以此声称召回率/答案质量提升；用户配置与参考
  语料未改。
- 完整差异已核对，`git diff --check` 与 `git diff --cached --check` 通过。实现提交
  为 `d950678`（`feat(plan233): forward search constraints and preserve source dates`）。

## 追加真实 API 验证（2026-10-19）

用户追问真实测试后，按既有授权读取本机 Tavily 配置，直接调用提交后的
`WebTools::search` 与生产 Tavily 适配器；临时探针、结果和原始响应仅留在临时目录，
密钥不落测试文件、输出或提交。共九次生产实现调用（八组加一次复测）及四次直连
对照，十三次均成功；没有修改运行时代码。

- 相同 `Rust release 1.90.0` 查询，不限域名的前五条仅一条来自 Rust 官方博客；
  指定该博客后服务端直接返回五条本站候选，排除该博客则五条均来自其他站点。
  本地过滤前的候选已符合条件，可证明这个样本的站点召回范围改善，不能外推总体
  搜索质量。
- 指定 2025-09-01 至 2025-09-30 返回三条，返回日期均在范围内；只给结束日期
  2020-12-31 返回五条，均不晚于边界。只给起始日期首轮为空，原请求直连和生产
  实现复测均返回五条且日期合规；更换关键词或补结束日期的对照也有结果。观察到
  服务端检索波动，未因此放宽过滤或增加自动重试。
- 已知精确短语返回三条，不存在的随机短语返回零条。十三次请求共返回五十个
  候选（含重复），均保留服务端日期；日期仍是服务端估计，非原始发布时间证明。
- 首轮八组耗时约 2.4–4.5 秒；仅为本机本次端到端样本，不作为性能保证。
  本机配置只提供 Tavily，此次未验证 Brave，也未测试尚未接入的新后端。
- 追加记录是纯文档变更，完整核对差异，并通过未暂存与暂存后的 diff 检查；
  不因文档记录重跑构建或测试。
