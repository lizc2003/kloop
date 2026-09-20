# Plan 171 — 搜索 key 回到配置文件里

> 来源:2026-09-20,用户贴出自己 `~/.kloop/config.toml` 里的
> `[web] search_provider = "tavily"` 问「这个配置的 key 在哪?我需要 config 自洽」。

## 一、不自洽是真的

同一个 `~/.kloop/config.toml`(0700 目录 + 0600 文件)里:

- **模型 provider 的凭据写在文件里**:`[providers.x].auth_header = { x-api-key = "..." }`。
- **搜索 backend 的凭据写不进去**:`[web]` 这张表只认 `search_provider` 一个键,别的键直接
  报错;key 只从 `TAVILY_API_KEY` / `BRAVE_API_KEY` 读。

于是 `[web] search_provider = "tavily"` 这两行**单独看是一句没有下文的话**——它声明了用哪个
后端,却没有任何位置说明这个后端怎么鉴权。用户照着配置文件配完,`web_search` 照样不注册,只在
启动时飘一条 `TAVILY_API_KEY not set`。**知道该 export 哪个变量的人能用,不知道的人得不到
任何提示**——而"该知道哪个变量名"本身就是 kloop 内部知识,不是用户该背的。

## 二、裁决:`api_key`,且不读环境变量

### 字段名用 `api_key`,不用 `auth_header`

provider 那边之所以让用户写 header,是因为**同一条 wire 可以指向任意网关,header 拼法是端点
的属性**——`x-api-key` 和 `Authorization: Bearer` 都合法,kloop 没资格替用户定
(`provider_config.rs` 的 `parse_auth_header` 文档注释就是这么写的)。

搜索后端没有这层自由度:base URL 写死在 `search.rs`(`with_base` 只是测试缝),没有网关概念,
header 由 `search_provider` 唯一确定——tavily 是 `bearer_auth`,brave 是
`X-Subscription-Token`。让用户在 `[web]` 里写 `auth_header`,等于**逼他填一个自己无权选择的
值**:多一处写错就启动报错的地方,还凭空造出第二份真相(`search_provider = "tavily"` 配
`X-Subscription-Token` 时,要么再写一条一致性校验,要么静默发错 header)。

`api_key` 只表达秘密,header 形态归实现,和 `search_provider` 不可能打架。

### 环境变量整个取消,不做 fallback

`TAVILY_API_KEY` / `BRAVE_API_KEY` **不再读**。理由是同一条:**用户不知道这些知识**。留一条
env 覆盖链看着"兼容",实际效果是把"我的 key 明明写了怎么没生效"变成一个需要懂优先级才能排查
的问题。配置文件是唯一来源,读不到就是读不到,warning 直接指向该写的那一行。

`core/src/tools/bash.rs` 的 `MODEL_SHELL_SECRET_ENV` **保留**这两个变量名:kloop 不读了,但
用户机器上别的工具可能仍 export 它们,从模型 shell 里剥掉照样是对的。

## 三、改动

- `WebConfig` 加 `api_key: Option<String>`;**撤掉 `derive(Debug)` 改手写**,key 渲染成
  `<redacted>`——这个结构体从此装凭据。
- `load_web_config` 认 `api_key`,空串报错(对齐 `auth_header.x-api-key = ""` 的既有拒绝),
  错误文案不回显值。
- `build_web_source` 不再碰 `std::env`;provider 名单只在一处列出,未知 provider 的
  warning 优先于缺 key 的 warning(provider 写错是更根本的错)。
- README 的 web_search 段同步。

## 四、完成记录

## ✅ 已完成(2026-09-20;提交 SHA 以本条所在提交为准)

`cli/src/web.rs` 一处改完,别处只是跟着说话:

- `WebConfig { search_provider, api_key: Option<String> }`。**`derive(Debug)` 撤掉改手写**,
  key 渲染成 `Some("<redacted>")`——它从此是个凭据容器,而断言和以后的日志都走 `{:?}`。
- `load_web_config` 认 `api_key`;空串报 `[web].api_key must not be empty`;未知键的报错列出
  两个合法键且只回显键名,不回显值。
- `build_web_source` 不再出现 `std::env`。provider 名单只在一处 match 里列出(`tavily` /
  `brave` / `other`),两个已知分支共用 `keyed()` 取 key,未知 provider 的 warning 排在缺 key
  的前面。
- `main.rs` 那句 "`--mock` 不读 web key" 的注释跟着改成"不起子进程、不接网络工具"——mock 下
  `UserConfig` 本来就是空表,旧说法的依据已经不在了。
- README web_search 段、HANDOFF web 段同步。`bash.rs` 的 `MODEL_SHELL_SECRET_ENV` 按裁决保留
  两个变量名。

### 测试(`crates/cli/src/web.rs`,5 个)

- `load_web_config_defaults_and_override` 补两组整对象断言:provider + key 都写,以及**只写
  key**(provider 落默认值,这才是最常见的那份配置)。
- `debug_never_prints_the_key`:两条整串断言,钉死 redact 与 `None` 两种渲染。
- `load_web_config_rejects_unknown_keys_and_bad_types` 补 `api_key = 3` / `api_key = ""`,
  外加 `api_keys = "tvly-sentinel"`——**拼错的键名不能把密钥回显进错误里**,整串断言。
- `build_web_source_degrades_search_by_provider` 改写成
  `build_web_source_registers_search_only_with_a_known_provider_and_a_key`。不读 env 之后注册
  结果是配置的纯函数,**三条路径这才第一次全部可断言**:有 key 注册 `[web_fetch, web_search]`
  且零 warning;缺 key 只剩 `web_fetch` + 指向 `~/.kloop/config.toml` 的那条 warning;provider
  写错时即使 key 也缺,报的仍是 provider。旧测试只敢测未知 provider 一条——另外两条当年都取决于
  跑测试的机器上有没有 `TAVILY_API_KEY`。

`cargo fmt --check` 干净,`clippy --all-targets -- -D warnings` 全绿,`cargo test`
**1601 passed / 0 failed**。

教训 167(两条:配置要能在它自己那一处配齐;只让用户填他真有权选择的东西)。

### 使用者须知

本机 `~/.kloop/config.toml` 的 `[web]` 段现在要补一行 `api_key = "tvly-..."`,否则下次启动
`web_search` 不再注册——原先 export 的 `TAVILY_API_KEY` 不再被读。
