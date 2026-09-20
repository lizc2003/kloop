# Plan 167 — 凭证是一个凭证,不是一袋 header

> 来源:2026-09-20,用户把自己 `~/.kloop/config.toml` 里新加的 gw-claude profile 和它的
> 报错贴过来:`providers.gw-claude.http_headers contains unsupported header 'Authorization'`。
> 顺着报错一路问下来,拍板三件事(问答过程见「裁决」各节)。

## 现状

```toml
[providers.gw-claude]
wire_api = "messages"
base_url = "https://gw-us.example.com/v1"
http_headers = { Authorization = "Bearer xxxxx" }
```

`parse_headers`(`cli/src/provider_config.rs`)每条 rail 只放行一个头:messages 收
`x-api-key`,chat/responses 收 `authorization`。下游 `anthropic::stream` 也写死
`.header("x-api-key", key)`。所以这个网关——Anthropic Messages 线协议 + Bearer 认证——
在 kloop 里根本配不出来。

## 裁决

### 一、限制留着,但限错了地方

用户问「需要限制头吗」。**封闭集合要留**,四条理由:

1. **脱敏只认一个 secret。** `send_checked(req, label, secret)` 只收一个 secret,
   `redact_secret` 拿它做全文替换。允许任意透传,第二个带密头就不在脱敏范围内——而
   `stream.rs` 那句"SSE error frame 和 HTTP body 一样能回显你的 Authorization"的整条防线,
   前提正是**密钥已知且唯一**。
2. **可用性判定靠它。** `selected_credential` → `ProviderAvailabilityCode::MissingCredential`。
   头一旦任意,"这个 profile 能不能用"就没有定义,未选中的 profile 也无法在 `/provider` 里标状态。
3. **协议自有的头会被顶掉。** `anthropic-version`、`x-claude-code-session-id`、content-type。
   放开之后配置能静默覆盖它们。
4. **没有需求。** provider crate 的接口是 `key: &str` 不是 header map,透传要把 HeaderMap
   穿过三条 rail。plan 158 砍 `query_params` 用的是同一条理由。

plan 40 当初的原话就是「不做任意 header 透传」(`docs/plan/40:31`),该判断仍然成立。

错的是另一半:**messages 线协议在野外本来就有两种凭证形态**,Anthropic 官方 SDK 自己就是
`apiKey → x-api-key` / `authToken → Authorization: Bearer`
(`refs/claude-code/dist/chunk-z0csm2zq.js:3886`),Claude Code 靠 `ANTHROPIC_AUTH_TOKEN`
接第三方网关走的正是后者。只收 x-api-key 拦不住任何东西,只是把这类网关挡在外面。

### 二、名字要改:`http_headers` → `auth_header`

用户:「那就不应该叫 http_headers,太通用了」。对——它收的不是一组头,是**一个凭证**;
而同一个配置文件里 MCP 侧的 `http_headers` 是真·任意附加头(`README:1184` 的
`X-Tenant = "acme"`),同名不同义。

我先提的是两个标量键 `api_key` / `auth_token`(对齐 Anthropic SDK 与
`ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN`,顺手把 `"Bearer "` 前缀这道仪式删掉)。
用户反提 `auth_header`,**采纳**,两条理由压过了"少打一个前缀":

1. **拼写属于网关,不属于 kloop 的内置知识。** `api_key`/`auth_token` 等于在代码里埋一张
   "这个词 → 那个头"的小表。kloop 的定位是对接任意第三方网关,plan 158 拒绝内置模型知识表
   用的就是同一条理由。Azure OpenAI 的认证头是 `api-key:`,既不是 `x-api-key` 也不是 Bearer;
   真要接时 `auth_header` 只是放行一个拼写,标量键则要新开一个配置键。
2. **看配置就知道线上发了什么。** 网关回 401 时 `auth_header = { Authorization = "Bearer x" }`
   自己就是答案;`auth_token = "x"` 得翻文档确认 kloop 发了哪个头。

形状:

```toml
[providers.gw-claude]
wire_api    = "messages"
base_url    = "https://gw-us.example.com"
auth_header = { Authorization = "Bearer xxxxx" }
```

- **表,不是字符串。** `"Authorization: Bearer x"` 要自己解析冒号,TOML 原生能表达的东西不自造语法。
- **单数 = 恰好一个。** 写了两条报错。
- **放行集合仍封闭**:`x-api-key` 与 `Authorization: Bearer`,**任何 rail 都收**,`match wire` 删掉。
  "可扩展"的意思是将来有真网关需要时加一个拼写、**连同它的密钥提取规则**,不是敞开透传。
- 密钥提取规则跟着拼写走:`redact_secret` 要拿到**裸 token** 才脱敏得干净,所以 `Authorization`
  必须剥掉 `Bearer ` 前缀,`x-api-key` 整个值就是密钥。加新拼写时这一条要一起想。

### 三、env 覆盖换密钥,不换形态

`ANTHROPIC_API_KEY`/`OPENAI_API_KEY` 覆盖的是**密钥**。profile 声明了 Bearer 时,env 来的
密钥照样按 Bearer 发——形态是网关的属性,跟密钥从哪儿来无关。profile 没声明 `auth_header`
时按 rail 默认形态(messages → x-api-key,chat/responses → Bearer),即今天的行为。

### 四、base_url 不统一带 `/v1`,也不加校验

用户问「统一带 v1 是不是更合适」。**不统一**:`ANTHROPIC_BASE_URL` 不是 kloop 发明的变量名,
Anthropic SDK / Claude Code / 照它们写文档的网关,这个值的约定都是不带 `/v1`(SDK 自己拼
`/v1/messages`);OpenAI SDK 的 baseURL 约定则是带 `/v1`。kloop 今天的"不对称"正是原样继承
两家约定(`Rail::default_base` 两行就是证据)。统一之后 kloop 的 `ANTHROPIC_BASE_URL` 就和
生态同名变量含义不同,而用户 shell 里那个变量很可能是为 Claude Code 导出的。

我接着提"那就在 startup 时拒绝 messages rail 以 `/v1` 结尾的 base_url",**用户否掉**:
「这种限制太大,业务场景有千变万化」。成立,而且能说清边界——**fail-closed 该用在失败是静默的
地方**(未知配置键会悄悄丢一个 server、凭证形态错会悄悄不发认证);URL 拼错不是那种,网关
自己会喊 404。用一条猜出来的规则拒绝一份本来能跑的配置,代价比让它 404 大。

### 五、改成让那声 404 说人话

上一条的替代,**不是限制**:provider 的 HTTP 错误现在是 `anthropic http 404: <body>`
(`stream.rs`),不带 URL。用户这次要不是去翻 `lib.rs` 的 `format!("{base}/v1/messages")`,
光看 404 看不出请求的是 `/v1/v1/messages`。`validate_base_url` 已经禁了 userinfo 和 query,
URL 里没有秘密,进错误信息是安全的。`send_checked` 已有 url 在手的三个调用点全覆盖,
超时与传输失败一并带上。

## 改动

1. **`crates/provider`**:新增 `AuthScheme { ApiKey, Bearer }` + `Credential { scheme, secret }`
   (构造器 `api_key`/`bearer`,`apply(RequestBuilder)`,`secret()`)。`Provider` 的三个真实
   变体把 `key: String` 换成 `cred: Credential`——密钥和它的发法必须绑在一起走,免得新增
   rail 时漏掉脱敏。`anthropic::stream`/`responses::stream`/`openai::stream` 收
   `&Credential`,由 `apply` 发头、`secret()` 喂 `send_checked`。
2. **`crates/provider/src/stream.rs`**:`send_checked` 增 `url` 参数,进 http/timeout/transport
   三条错误信息。
3. **`crates/cli/src/provider_config.rs`**:`http_headers` → `auth_header`(`parse_profile`
   的封闭键表同改,旧名走既有的 unknown key 报错,**不留兼容别名**——用户明确不要兼容);
   `parse_headers` → `parse_auth_header`,返回 `Option<Credential>`,恰好一条、两种拼写、
   任何 rail;`Profile.headers` → `Profile.auth: Option<Credential>`;`selected_credential`
   返回 `Option<Credential>`,env 只换密钥;`bearer_key` 删,`bearer_from_header` 留给解析。
4. **README**:provider schema 注释里的 `http_headers` 改 `auth_header`,补两种拼写与
   "任何 rail 都收"、env 只换密钥。MCP 侧的 `http_headers` 不动(那里确实是任意头)。
5. **本机 `~/.kloop/config.toml`** 三个 profile 跟着改(不进 git)。

## 非目标

- **不补 `ANTHROPIC_AUTH_TOKEN`。** env-only 路径(无配置文件,CI 入口)目前只能是 rail 默认
  形态。要 Bearer 就写配置文件。真需要时再说。
- **不做任意 header 透传**(见裁决一)。
- **不碰 MCP 的 `http_headers`。** 它那边凭证是独立的 `bearer_token_env_var`,槽和附加头本来
  就是两个字段——provider 侧将来真需要附加头,照它加一个独立字段,而不是把凭证槽敞开。

## 完成标准

- fmt / clippy `-D warnings` / 全 workspace test 全绿;`cargo run -p kloop -- --mock` 可跑。
- 测试覆盖:两种拼写各一条解析用例、两条同写报错、非法拼写报错、`Authorization` 缺 Bearer 报错、
  env 覆盖不改形态、messages rail 真发 `Authorization: Bearer`、404 错误信息含 URL。
- README、本 plan ✅ 与提交号、HANDOFF 教训一并更新,一次 commit。

## 完成记录 ✅(2026-09-20)

- **provider crate**:新增 `AuthScheme { ApiKey, Bearer }` 与 `Credential`(`api_key`/`bearer`
  构造器、`scheme()`、`apply(RequestBuilder)`、`secret()`)。`Provider` 的 Anthropic /
  OpenAiCompat / OpenAiResponses 三个变体把 `key: String` 换成 `cred: Credential`——密钥和它
  的发法绑在一起走,`apply` 发头、`secret()` 同时喂 `send_checked` 与 `error_detail`,漏脱敏
  这条路被类型堵死。
- **`stream.rs`**:`send_checked` 增 `url` 参数,进 http / timeout / transport 三条错误消息。
- **cli**:`auth_header` 取代 `http_headers`(封闭键表照旧,旧名落 `unknown key`,无别名);
  `parse_auth_header` 返回 `Option<Credential>`,恰好一条、两种拼写、三条 rail 都收、
  `Authorization` 必须是 Bearer 且只存裸 token;`Profile.auth`;`Rail::default_auth()`;
  `selected_credential` 返回 `Option<Credential>`,env 只换密钥不换形态;删 `bearer_key`。
- **测试**:`auth_header_takes_either_spelling_on_any_rail`(两种拼写 + 大小写 + 未声明 +
  messages/Bearer 走通 `resolve`)、`auth_header_is_exactly_one_known_spelling`(未知拼写 /
  两条 / 缺 Bearer / 空值 / 非表 / 空表 / 旧名)、
  `environment_credentials_replace_the_secret_not_the_spelling`(声明过的保形态、未选中不受
  影响、未声明落 rail 默认、chat rail 默认 Bearer)、
  `the_credential_spelling_travels_with_the_credential`(wiremock 只应答
  `authorization: Bearer test-key`)、`http_failures_name_the_endpoint_they_were_sent_to`。
- **README**:schema 注释改 `auth_header`,补"一个凭证不是一袋头 / 两种拼写 / 任何 rail 都收 /
  写两条报错 / 集合为何封闭",以及 env 只换密钥不换形态。
- 本机 `~/.kloop/config.toml` 三个 profile 已是新形状(gw-claude 的 `base_url` 同时去掉了多余
  的 `/v1`),0600 未变,不进 git。
- 验证:`cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`、
  `cargo test --workspace` 全绿;`cargo run -p kloop -- --mock --headless` 跑通。
  **真实网关未验**——gw-claude 的实跑要花用户的 key/额度,单独确认。
- 提交:本次(plan 167,见 git log)。
