# Plan 261 — web_fetch:解析固定、字符集与早拒

> 2026-10-28 立。来源：全仓只读审查(HANDOFF「〇⁵」节)。
> 状态：待实现。行号是 2026-10-28 的读数，开工先自己量。

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
