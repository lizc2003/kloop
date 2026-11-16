# Plan 261 — web_fetch:解析固定、字符集与早拒

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：✅ 已实现（2026-11-16）。行号是 2026-10-28 的读数；实际路径已迁到
> `rust/crates/web/`，开工按函数定位。

## 问题

1. **SSRF 守卫有 DNS rebinding 窗口**(已确认，注释自认 "accepted TOCTOU")。
   `web/src/fetch.rs:315-345` `guard()` 先 `tokio::net::lookup_host` 校验，reqwest 连接时再解析一次;
   client(`web/src/lib.rs:51-58`)没接入自定义解析器。第一次解析返回公网 IP、TTL 0,第二次返回
   `127.0.0.1` 或 `169.254.169.254`,`web_fetch` 就连到内网。
2. **完全不看 charset**(已确认)。`fetch.rs:247` `String::from_utf8_lossy(&bytes)`:
   `Content-Type: text/html; charset=gbk`、Shift_JIS、ISO-8859-1 的页面输出大量 U+FFFD,看起来
   却像抓取成功。DESIGN 里没有 charset 的说明。
3. **先下载满 body 才检查 content-type**(优化)。`fetch.rs:252`:指向 ISO/zip 的 URL
   (`application/octet-stream`)先下 20MiB 才报 unsupported。

## 开工先问用户

- 第 2 条要用 `encoding_rs` 吗?它已在 `Cargo.lock` 里(作为传递依赖),直接依赖不增加编译成本，
  但按仓库惯例新增直接依赖要在 workspace `Cargo.toml` 写明理由。推荐用;也可以打开 reqwest 的
  `charset` feature 交给 `Response::text()`,但那样拿不到 `<meta charset>` 的回退。

## 做法

1. 实现 `reqwest::dns::Resolve`,经 `ClientBuilder::dns_resolver` 接入，在实际连接用的解析结果上
   用 `ip_is_public` 过滤;校验与连接用同一次解析。重定向的每一跳都经同一解析器(开工核对
   reqwest 的重定向是否复用 resolver)。删掉"accepted TOCTOU"注释。
2. 从 Content-Type 取编码，HTML 再看 `<meta charset>`/`http-equiv`;不认识的编码退回 lossy,
   并在结果里注明。
3. content-type 非空且不是文本类型时，读 body 之前就拒绝;空 content-type 仍走现在的嗅探。

## 验收

- 单测(旧代码先红):解析器对私网/回环/链路本地地址返回错误(不起真实 DNS,用可注入的解析源);
  GBK 与 Shift_JIS 页面解码正确;`application/octet-stream` 在读 body 前被拒(夹具记录读取字节数)。
- DESIGN 的 web_fetch 一节补字符集与解析固定;外层执行沙箱之外 `make check`;HANDOFF 记进展。

## 完成记录（2026-11-16）

- **复核修正（同日，三处）**：
  - 预检解析失败改回**拒绝**：本机解析不了的名称不再交给代理去解析（代理可能把它解进内网，
    而连接期过滤器只看得到代理地址）。`guard` 与 client 现在共用同一个可注入的
    `PublicResolver`，预检与连接使用同一 DNS 源，测试可用脚本化答案复现 rebinding。
  - 代理端点豁免：从 `ALL_PROXY` / `HTTPS_PROXY` / `HTTP_PROXY`（大小写两种）、
    `host:port` 无 scheme 写法读取代理主机，与 reqwest 一致（CGI 下忽略），这些主机解析到
    私网/回环地址不再被拒，否则 `HTTPS_PROXY=http://localhost:7890` 会让全部 web 工具不可用。
  - `<meta>` 预扫描跳过 HTML 注释：注释里的编码声明不再盖掉其后的真实声明
    （原先会把 UTF-8 页面按 windows-1252 解成无告警的乱码）。
- **第二轮复核修正（同日）**：
  - 代理豁免绑定到代理腿：按名字豁免会让 `NO_PROXY` 命中时的直连目标也跳过地址检查。
    现在配置的代理端点本身被当作非抓取目标直接拒绝，fetch 的每一跳都必经此门，于是叫
    这个名字的连接只可能是 reqwest 走代理的那一条；代理腿的私网地址仍被豁免。
  - 注释识别只在正文生效：`<!--` 位于带引号的属性值里不算注释（预扫描与 HTML→text 的
    注释剥离都改了），否则属性里的 `<!--` 会连同其后的真实 `<meta charset>` 一起被丢掉。
- **第三轮复核修正（同日）**：注释剥离退化成平方复杂度——`tag_end` 在非标签起点也被
  调用，正文里大量孤立 `<`（无 `>`）时每个 `<` 都重扫整个尾部。现在只在标签起点扫描，
  标签不闭合即结束；回归用 1M 个孤立 `<` 在 5s 超时内完成来证明不再整段重扫。
- **解析固定**：`PublicResolver` 实现 `reqwest::dns::Resolve`，在 `WebTools` 的 client 上经
  `dns_resolver` 接入；不在公网范围内的答案在连接实际使用的那次解析里被拒，每跳重定向经同一
  client 与解析器，`accepted TOCTOU` 注释已删。每跳的静态检查与目标名预检保留：http 代理下
  reqwest 只解析代理，目标若本地解析为私网地址只有这一步能看见。测试用可注入的 `Lookup`，
  不起真实 DNS。
- **字符集**：直接依赖 `encoding_rs`（用户 2026-11-16 同意）。Content-Type 的 `charset` 优先，
  HTML 再看头部 1024 字节内的 `<meta charset>` / `http-equiv`，BOM 由解码库优先于两者；未声明
  编码仍按 UTF-8 有损解码；未知标签或字节不合声明编码时退回有损解码并在正文尾部注明。
- **早拒**：非文本 Content-Type 在 `read_body` 之前拒绝，空类型仍按正文开头嗅探；夹具只发
  响应头、不承诺正文，断言客户端未等正文就关闭连接。
- **验证**：`cargo test -p kloop-web --locked --offline`（47 passed，1 ignored 的 20 MiB
  capped 用例未跑；含 rebinding、不可解析目标、代理豁免、代理端点拒抓、注释 meta、
  属性内 <!-- 与孤立 `<` 扫描七条回归）、
  `cargo clippy -p kloop-web --all-targets -- -D warnings`、
  `cargo fmt -p kloop-web -- --check`。按 2026-11-06 验证约定未跑 `make check`：改动只落在
  `kloop-web` 及其依赖声明，局部用例覆盖了三条行为；`WebTools` 的其它调用方只有本 crate 的
  search 集成测试，也已一起跑过。
