//! web_fetch: manual redirect following with a per-hop SSRF guard, a download
//! cap, HTML→text conversion. There is deliberately no model-text cap — see
//! `read_body`.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use encoding_rs::Encoding;
use reqwest::Url;
use reqwest::dns::Addrs;
use reqwest::dns::Name;
use reqwest::dns::Resolve;
use reqwest::dns::Resolving;

use crate::limits;

const MAX_REDIRECTS: usize = 5;

pub(crate) async fn fetch_url(
    client: &reqwest::Client,
    dns: &PublicResolver,
    raw_url: &str,
    allow_private: bool,
) -> Result<String> {
    let mut url = normalize_url(raw_url, /*upgrade_http*/ !allow_private)?;
    let requested = url.clone();
    for _ in 0..=MAX_REDIRECTS {
        guard(dns, &url, allow_private).await?;
        let resp = client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("web_fetch: request to {url} failed"))?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .with_context(|| format!("web_fetch: redirect from {url} without Location"))?;
            let target = url
                .join(location)
                .with_context(|| format!("web_fetch: invalid redirect target '{location}'"))?;
            // Cross-host redirects are surfaced, not followed (cc shape):
            // the model must opt into the new host with a fresh call, which
            // kills open-redirect laundering through an approved URL.
            if !same_site(&url, &target) {
                return Ok(format!(
                    "Redirect detected: {url} redirects to {target}. \
                     Fetch that URL explicitly if you want to follow the redirect."
                ));
            }
            url = target;
            continue;
        }
        if !resp.status().is_success() {
            let guidance = match resp.status().as_u16() {
                401 => {
                    "Authentication is required. Use an existing authenticated client or ask for access; do not search credential files."
                }
                403 => {
                    "The server refused this request. Use an already authorized access route or ask for access; do not bypass the refusal."
                }
                404 if pull_request_url(&url) => {
                    "This looks like a pull request URL. The resource may be missing or hidden from unauthenticated requests.\nCompare it with the current checkout's Git remotes; a matching remote may provide existing authenticated access — use git ls-remote to query PR refs and git fetch for missing objects. Avoid git pull, which can modify the current branch and working tree. PR head refs do not establish the target branch.\nOtherwise use an existing authenticated client or ask for the missing information. Do not guess alternate URLs or IPs, or search credential files."
                }
                404 => {
                    "The resource may be missing or hidden from unauthenticated requests. Use an existing authenticated client or ask for the missing information. Do not guess alternate URLs or IPs, or search credential files."
                }
                410 => {
                    "The server reports that this resource is gone. Ask for its current location instead of repeatedly fetching this URL."
                }
                429 => {
                    "The server rate-limited this request. Respect Retry-After if available and retry later; changing URLs does not resolve rate limiting."
                }
                500 | 502 | 503 | 504 => {
                    "The server reported a failure; a later retry may help. Do not infer a missing resource or change the URL from this status alone."
                }
                _ => {
                    "The server returned an unsuccessful HTTP response. Confirm the requested resource and access route before retrying."
                }
            };
            let mut message = format!("web_fetch: HTTP {} for {url}\n{guidance}", resp.status());
            // The 429 guidance asks the model to respect Retry-After, so the
            // value has to reach it.
            if resp.status().as_u16() == 429
                && let Some(value) = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
            {
                message.push_str(&match usable_retry_after(value) {
                    Some(value) => format!("\nRetry-After: {value}"),
                    None => "\nRetry-After: (value too long to be one; ignored)".into(),
                });
            }
            bail!("{message}");
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        // Refuse a body this tool cannot read before paying for the transfer:
        // a link to a PDF or an archive used to download the full 20 MiB first.
        if !is_text_content_type(&content_type) {
            bail!(
                "web_fetch: unsupported content type '{content_type}' at {url} (text-like content only)"
            );
        }
        let mut text = read_body(resp, &content_type, &url).await?;
        // A same-site redirect is followed silently, so without this the body
        // of a login or interstitial page arrives looking like the document
        // that was asked for — a false success, worse than the 404 it hides.
        if url != requested {
            text.push_str(&format!("\n\n[followed same-site redirect to {url}]"));
        }
        return Ok(text);
    }
    bail!("web_fetch: too many redirects (more than {MAX_REDIRECTS})")
}

/// The `Retry-After` value as it should reach the model. `to_str` has already
/// restricted it to visible ASCII; a legal value is delta-seconds or an
/// HTTP-date, both far inside this width, so anything longer is not a value
/// worth repeating (and truncating it would invent a different one).
fn usable_retry_after(value: &str) -> Option<&str> {
    (!value.is_empty() && value.len() <= 64).then_some(value)
}

/// Whether the URL has the shape of a pull-request page:
/// `<host>/[…/]<owner>/<repo>/pull|pulls/<number>`, with or without a trailing
/// slash. Only this shape earns the Git hints on a 404 — a forge allowlist
/// would age, and without one a two-segment repository path is
/// indistinguishable from any other URL. The hints stay host-agnostic so a
/// self-hosted forge is covered too.
fn pull_request_url(url: &Url) -> bool {
    let Some(segments) = url.path_segments() else {
        return false;
    };
    let segments: Vec<&str> = segments.filter(|part| !part.is_empty()).collect();
    let [.., kind, number] = segments.as_slice() else {
        return false;
    };
    matches!(*kind, "pull" | "pulls")
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// Parse + static hygiene: length cap, no embedded credentials, http→https
/// upgrade (cc shape; skipped in tests where the mock server is plain http).
fn normalize_url(raw_url: &str, upgrade_http: bool) -> Result<Url> {
    if raw_url.len() > 2000 {
        bail!("web_fetch: URL is longer than 2000 characters");
    }
    let mut url =
        Url::parse(raw_url).with_context(|| format!("web_fetch: invalid URL '{raw_url}'"))?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("web_fetch: URLs with embedded credentials are not allowed");
    }
    if upgrade_http && url.scheme() == "http" {
        let _ = url.set_scheme("https");
    }
    Ok(url)
}

/// Same scheme, port and host (modulo a `www.` prefix) — the only redirects
/// followed silently.
fn same_site(from: &Url, to: &Url) -> bool {
    let strip_www = |u: &Url| {
        u.host_str()
            .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
    };
    from.scheme() == to.scheme()
        && from.port_or_known_default() == to.port_or_known_default()
        && strip_www(from).is_some()
        && strip_www(from) == strip_www(to)
        && to.username().is_empty()
        && to.password().is_none()
}

/// Content types this tool treats as text.
///
/// Text is named exactly — the spellings servers actually send, including the
/// `x-` aliases — or through its structured-syntax suffix. It is never inferred
/// from a marker inside the type, because a marker does not imply text: `xml`
/// appears in the binary OOXML documents
/// (`application/vnd.openxmlformats-officedocument.wordprocessingml.document`),
/// `sql` in `application/x-sqlite3`, `x-sh` in `application/x-sharedlib`. Each
/// of those reached the model as lossy-decoded bytes under a marker test.
/// Parameters (`;charset=…`) are dropped first, so they cannot carry one in.
fn is_text_content_type(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or_default().trim();
    if essence.is_empty() || essence.starts_with("text/") {
        return true;
    }
    // The structured-syntax suffix of a text format: `+json`, `+xml`, `+yaml`…
    if TEXT_SUFFIXES.iter().any(|suffix| essence.ends_with(suffix)) {
        return true;
    }
    TEXT_TYPES.contains(&essence)
}

/// Text media types worth naming: the registered spellings plus the `x-` aliases
/// and legacy names hosts still serve. A type that is neither here nor a
/// structured suffix is refused — for a body this tool cannot tell from binary,
/// the unsupported-type answer is the honest one.
const TEXT_TYPES: &[&str] = &[
    "application/json",
    "application/x-json",
    "application/json5",
    "application/ndjson",
    "application/x-ndjson",
    "application/json-seq",
    "application/xml",
    "application/x-xml",
    "application/xml-dtd",
    "application/javascript",
    "application/x-javascript",
    "application/ecmascript",
    "application/x-ecmascript",
    "application/yaml",
    "application/x-yaml",
    "application/toml",
    "application/x-toml",
    "application/sh",
    "application/x-sh",
    "application/x-shellscript",
    "application/sql",
    "application/x-sql",
    "application/csv",
    "application/x-csv",
    "application/markdown",
    "application/x-markdown",
];

const TEXT_SUFFIXES: &[&str] = &["+json", "+xml", "+text", "+yaml", "+toml"];

async fn read_body(mut resp: reqwest::Response, content_type: &str, url: &Url) -> Result<String> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut download_truncated = false;
    loop {
        let next = resp
            .chunk()
            .await
            .with_context(|| format!("web_fetch: reading body from {url} failed"))?;
        let Some(chunk) = next else {
            break;
        };
        bytes.extend_from_slice(&chunk);
        if bytes.len() > limits::MAX_DOWNLOAD_BYTES {
            bytes.truncate(limits::MAX_DOWNLOAD_BYTES);
            download_truncated = true;
            break;
        }
    }
    let is_html = content_type.contains("text/html")
        || content_type.contains("application/xhtml")
        || (content_type.is_empty() && looks_like_html(&bytes));
    let decoded = decode_body(&bytes, content_type, is_html);
    // Sampled before `decoded.text` can be moved by the non-HTML branch below.
    let html_bytes = if is_html { decoded.text.len() } else { 0 };
    let html_has_script = is_html && decoded.text.contains("<script");
    let mut text = if is_html {
        crate::html::html_to_text(&decoded.text)
    } else {
        decoded.text
    };
    if text.is_empty() {
        text = "(empty response body)".into();
    }
    if is_html && is_client_rendered(html_bytes, html_has_script, &text) {
        text.push_str(CLIENT_RENDERED_NOTE);
    }
    if let Some(note) = decoded.note {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    // No model-text clip here. It used to cut at 50k chars and drop the rest,
    // which duplicated the offload seam while destroying what that seam exists
    // to preserve: `History::record` already spills an oversized tool result to
    // the session store and hands the model a preview plus pointer, so the whole
    // body stays reachable. Dogfooding a review against
    // https://api.elevenlabs.io/openapi.json (2,106,298 bytes) kept 50,000 chars
    // — 2.4%, the head — and `components.schemas` was past the cut with no way
    // back. The download cap below still bounds the transfer; model visibility
    // is the offload seam's job alone.
    if download_truncated {
        text.push_str(&truncation_marker());
    }
    Ok(text)
}

fn looks_like_html(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| *byte == b'<')
}

struct Decoded {
    text: String,
    /// Set when the declaration could not be honoured, so a lossy decode is
    /// never passed off as the page's own text.
    note: Option<String>,
}

/// Header charset first, then the HTML `<meta>` declaration; `encoding_rs` also
/// honours a BOM, which outranks both as it does in browsers. An unrecognised
/// label falls back to UTF-8 lossily and says so — the old unconditional
/// `from_utf8_lossy` turned a GBK or Shift_JIS page into U+FFFD noise that still
/// looked like a successful fetch.
fn decode_body(bytes: &[u8], content_type: &str, is_html: bool) -> Decoded {
    let declared = charset_from_content_type(content_type).or_else(|| {
        if is_html {
            html_meta_charset(bytes)
        } else {
            None
        }
    });
    let Some(label) = declared else {
        return Decoded {
            text: String::from_utf8_lossy(bytes).into_owned(),
            note: None,
        };
    };
    let Some(encoding) = Encoding::for_label(label.as_bytes()) else {
        return Decoded {
            text: String::from_utf8_lossy(bytes).into_owned(),
            note: Some(format!(
                "[declared charset '{label}' is not recognised: decoded as UTF-8 with \
                 replacement characters]"
            )),
        };
    };
    let (text, actual, had_errors) = encoding.decode(bytes);
    Decoded {
        text: text.into_owned(),
        note: had_errors.then(|| {
            format!(
                "[body contains bytes that are not valid '{}': decoded with replacement \
                 characters]",
                actual.name()
            )
        }),
    }
}

fn charset_from_content_type(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        let value = value.trim().trim_matches('"');
        (!value.is_empty()).then_some(value)
    })
}

/// The `<meta>` encoding declaration inside the first 1024 bytes — the prescan
/// window browsers use, so a declaration further down is not consulted.
/// Comments are skipped: a commented-out `<meta charset=…>` is not authoritative
/// and would otherwise override the real declaration that follows it.
fn html_meta_charset(bytes: &[u8]) -> Option<&str> {
    let head = &bytes[..bytes.len().min(1024)];
    let mut from = 0;
    loop {
        let meta = find_ascii_ci(&head[from..], b"<meta").map(|offset| from + offset);
        let comment = find_bytes(&head[from..], b"<!--").map(|offset| from + offset);
        if comment.is_some_and(|comment| meta.is_none_or(|meta| comment < meta)) {
            let comment = comment.unwrap();
            let end = find_bytes(&head[comment + b"<!--".len()..], b"-->")?;
            from = comment + b"<!--".len() + end + b"-->".len();
            continue;
        }
        let start = meta?;
        // `<metadata>` is not `<meta>`.
        if !head
            .get(start + b"<meta".len())
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        {
            from = start + b"<meta".len();
            continue;
        }
        let end = head[start..].iter().position(|&byte| byte == b'>')? + start;
        if let Some(label) = charset_attribute(&head[start..end]) {
            return Some(label);
        }
        from = end + 1;
    }
}

fn find_ascii_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Both spellings in the wild: `<meta charset=…>` and the `http-equiv`
/// `content="text/html; charset=…"`. The slice has already been narrowed to a
/// `<meta>` tag.
fn charset_attribute(tag: &[u8]) -> Option<&str> {
    let mut cursor = find_ascii_ci(tag, b"charset")? + b"charset".len();
    while tag
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    if tag.get(cursor) != Some(&b'=') {
        return None;
    }
    cursor += 1;
    while tag
        .get(cursor)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        cursor += 1;
    }
    if tag
        .get(cursor)
        .is_some_and(|byte| *byte == b'"' || *byte == b'\'')
    {
        cursor += 1;
    }
    let start = cursor;
    while tag.get(cursor).is_some_and(|byte| {
        !byte.is_ascii_whitespace() && !matches!(*byte, b'"' | b'\'' | b'>' | b';' | b'/')
    }) {
        cursor += 1;
    }
    std::str::from_utf8(&tag[start..cursor])
        .ok()
        .filter(|label| !label.is_empty())
}

/// What a fetch that hit the download bound appends. Derived from the bound so
/// raising it cannot leave a stale number behind, and it says the remainder is
/// not retrievable — otherwise a second fetch looks like a way to get it.
fn truncation_marker() -> String {
    format!(
        "\n\n[download truncated at {} MiB: more content was available and this tool cannot \
         retrieve it, so a second fetch would stop at the same bound]",
        limits::MAX_DOWNLOAD_BYTES / (1024 * 1024)
    )
}

/// Appended when the extracted text is a client-render placeholder rather than
/// the page. Without it the model gets a non-empty, plausible-looking body
/// ("You need to enable JavaScript to run this app.") with no way to tell a
/// short page from a failed fetch — measured cost on one session: two URLs
/// tried, then four rounds of web_search guessing at the content.
const CLIENT_RENDERED_NOTE: &str = "\n\n[this page is client-rendered: the HTML \
carries scripts but almost no text, so the content is not in the response body. \
Fetching this URL again returns the same shell — use web_search, or look for a \
static/raw version of the document]";

/// Conservative: all three must hold, so a genuinely short static page (no
/// scripts, or markup no larger than its text) is never flagged.
fn is_client_rendered(html_bytes: usize, has_script: bool, text: &str) -> bool {
    has_script && html_bytes > 1000 && text.trim().chars().count() < 200
}

/// Scheme and address policy for every redirect hop. Literal addresses are
/// judged directly. DNS names get two checks: this one, and the authoritative
/// one in [`PublicResolver`] on the addresses the connection actually uses.
/// The resolver alone is not enough — with an HTTP proxy configured reqwest
/// resolves only the proxy, so a name that resolves privately would never be
/// seen at all. A name this host cannot resolve is refused too: letting a proxy
/// resolve it is exactly the case where an internal target would otherwise
/// arrive without any local address check.
async fn guard(dns: &PublicResolver, url: &Url, allow_private: bool) -> Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("web_fetch: only http/https URLs are supported (got '{other}:')"),
    }
    if allow_private {
        return Ok(());
    }
    match url.host() {
        None => bail!("web_fetch: URL has no host"),
        Some(url::Host::Ipv4(ip)) if !ip_is_public(IpAddr::V4(ip)) => bail!(refuse_private(&ip)),
        Some(url::Host::Ipv6(ip)) if !ip_is_public(IpAddr::V6(ip)) => bail!(refuse_private(&ip)),
        Some(url::Host::Domain(name)) => {
            if is_localhost_name(name) {
                bail!(refuse_private(&name));
            }
            dns.check_target(name).await?;
        }
        Some(_) => {}
    }
    Ok(())
}

fn refuse_private(what: &dyn std::fmt::Display) -> String {
    format!("web_fetch: {what} is a private/internal address; refusing (SSRF guard)")
}

fn is_localhost_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("localhost") || name.to_ascii_lowercase().ends_with(".localhost")
}

/// DNS resolution for the HTTP client. Filtering here rather than before the
/// request closes the rebinding window: reqwest connects to exactly these
/// addresses, and every redirect hop is another request through this client.
pub(crate) struct PublicResolver {
    lookup: Arc<dyn Lookup>,
    /// Proxy endpoints from the environment. They are the user's declared
    /// egress route, not fetch targets, so the public-address filter must not
    /// block them — a local or corporate proxy commonly sits on a private
    /// address.
    proxy_hosts: Vec<String>,
}

impl PublicResolver {
    pub(crate) fn system() -> Self {
        Self {
            lookup: Arc::new(SystemLookup),
            proxy_hosts: proxy_hosts_from_env(),
        }
    }

    /// The per-hop target check. Fail-closed on a lookup error: a name this
    /// host cannot resolve may still be resolvable by the proxy, which is how
    /// an internal target could otherwise be reached without any local address
    /// check.
    async fn check_target(&self, host: &str) -> Result<()> {
        let addrs = self.answers(host).await.map_err(anyhow::Error::msg)?;
        public_answers(host, &addrs).map_err(anyhow::Error::msg)
    }

    async fn answers(&self, host: &str) -> std::result::Result<Vec<SocketAddr>, String> {
        let addrs = self
            .lookup
            .lookup(host.to_owned())
            .await
            .map_err(|err| format!("web_fetch: cannot resolve host '{host}': {err}"))?;
        if addrs.is_empty() {
            return Err(format!("web_fetch: host '{host}' resolves to no addresses"));
        }
        Ok(addrs)
    }

    fn is_proxy(&self, host: &str) -> bool {
        self.proxy_hosts
            .iter()
            .any(|proxy| proxy.eq_ignore_ascii_case(host))
    }
}

fn public_answers(host: &str, addrs: &[SocketAddr]) -> std::result::Result<(), String> {
    if addrs.iter().any(|addr| !ip_is_public(addr.ip())) {
        return Err(refuse_private(&host));
    }
    Ok(())
}

/// Proxy endpoints from the same variables reqwest reads, so a configured
/// proxy is reachable even when it resolves to a private address. reqwest
/// ignores all of them under CGI (to avoid httpoxy), and so does this.
fn proxy_hosts_from_env() -> Vec<String> {
    proxy_hosts(|name| std::env::var(name).ok())
}

fn proxy_hosts(env: impl Fn(&str) -> Option<String>) -> Vec<String> {
    if env("REQUEST_METHOD").is_some() {
        return Vec::new();
    }
    [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .into_iter()
    .filter_map(env)
    .filter_map(|value| proxy_host(&value))
    .collect()
}

/// A proxy value may omit the scheme (`host:port`), which `Url::parse` would
/// read as a scheme with no host; reqwest assumes `http` in that case.
fn proxy_host(value: &str) -> Option<String> {
    let value = value.trim();
    let url = Url::parse(value)
        .ok()
        .filter(|url| url.host_str().is_some())
        .or_else(|| Url::parse(&format!("http://{value}")).ok())?;
    url.host_str().map(str::to_owned)
}

/// The DNS source, split out so tests can answer without real DNS.
trait Lookup: Send + Sync {
    fn lookup(&self, host: String) -> LookupFuture;
}

type LookupFuture = Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>;

struct SystemLookup;

impl Lookup for SystemLookup {
    fn lookup(&self, host: String) -> LookupFuture {
        Box::pin(async move { Ok(tokio::net::lookup_host((host.as_str(), 0)).await?.collect()) })
    }
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let lookup = Arc::clone(&self.lookup);
        let is_proxy = self.is_proxy(&host);
        Box::pin(async move {
            if !is_proxy && is_localhost_name(&host) {
                return Err(io::Error::other(refuse_private(&host)).into());
            }
            let addrs = lookup.lookup(host.clone()).await.map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!("web_fetch: cannot resolve host '{host}': {err}"),
                )
            })?;
            if addrs.is_empty() {
                return Err(io::Error::other(format!(
                    "web_fetch: host '{host}' resolves to no addresses"
                ))
                .into());
            }
            if !is_proxy && addrs.iter().any(|addr| !ip_is_public(addr.ip())) {
                return Err(io::Error::other(refuse_private(&host)).into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn ip_is_public(ip: IpAddr) -> bool {
    fn v4_public(ip: Ipv4Addr) -> bool {
        let [a, b, c, _] = ip.octets();
        !(ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast()
            || ip.is_multicast()
            || ip.is_documentation()
            // 0.0.0.0/8 "this host on this network".
            || a == 0
            // 240.0.0.0/4 reserved (class E); 255.255.255.255 is broadcast above.
            || a >= 240
            // 100.64.0.0/10 (CGNAT) — covers most cloud metadata detours.
            || (a == 100 && (64..128).contains(&b))
            // 192.0.0.0/24 (protocol assignments).
            || (a == 192 && b == 0 && c == 0))
    }
    match ip {
        IpAddr::V4(v4) => v4_public(v4),
        IpAddr::V6(v6) => {
            // v6-native non-public ranges FIRST: `::1`/`::` are IPv4-compatible,
            // so `to_ipv4()` would map them to a public-looking `0.0.0.x` and
            // wave them through if checked before these.
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            let seg = v6.segments();
            // fc00::/7 unique-local, fe80::/10 link-local.
            if (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80 {
                return false;
            }
            // Embedded IPv4 — both ::ffff:a.b.c.d (mapped) and ::a.b.c.d
            // (compatible): judge the inner address, catching a loopback/
            // private/CGNAT hidden in v6 form.
            if let Some(v4) = v6.to_ipv4() {
                return v4_public(v4);
            }
            // NAT64 well-known prefix 64:ff9b::/96 embeds a v4 the gateway
            // routes to; judge the embedded address the same way.
            if seg[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
                let v4 = Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return v4_public(v4);
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {

    /// Only the recognisable pull-request shape earns the Git hints: a
    /// self-hosted path prefix and a trailing slash still count, while a bare
    /// repository path or a non-numeric suffix does not.
    #[test]
    fn pull_request_shape_is_recognised_narrowly() {
        let url = |text: &str| reqwest::Url::parse(text).unwrap();
        for text in [
            "https://forge.example/team/project/pulls/42",
            "https://forge.example/team/project/pulls/42/",
            "https://forge.example/git/team/project/pulls/42#comment",
            "https://github.com/team/project/pull/7",
        ] {
            assert!(super::pull_request_url(&url(text)), "{text}");
        }
        for text in [
            "https://forge.example/team/project",
            "https://forge.example/team/project.git",
            "https://forge.example/team/project/pulls",
            "https://forge.example/team/project/pulls/latest",
            "https://forge.example/team/project/issues/42",
            "https://forge.example/team/project/pull/42/files",
            "https://forge.example/",
        ] {
            assert!(!super::pull_request_url(&url(text)), "{text}");
        }
    }

    /// Text is named, never inferred from a marker: a marker does not imply
    /// text, and a list that only names the common spellings loses the aliases
    /// hosts still send.
    #[test]
    fn text_content_types_are_named_not_inferred() {
        for content_type in [
            "text/plain",
            "application/json",
            "application/x-ndjson",
            "application/json-seq",
            "application/x-javascript",
            "application/xml",
            "application/xml-dtd",
            "application/x-yaml; charset=utf-8",
            "application/toml",
            "application/x-sh",
            "application/x-shellscript",
            "application/sql",
            "application/schema+json",
            "image/svg+xml",
            "",
        ] {
            assert!(super::is_text_content_type(content_type), "{content_type}");
        }
        for content_type in [
            // Binary despite containing "xml" / "sql" / "x-sh".
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "application/x-sqlite3",
            "application/x-sharedlib",
            "application/x-shockwave-flash",
            "application/octet-stream",
            // A parameter must not be able to smuggle a marker in.
            "application/octet-stream; x=json",
            "application/pdf",
            "image/png",
        ] {
            assert!(!super::is_text_content_type(content_type), "{content_type}");
        }
    }

    /// Three signals must agree, so this never fires on a short static page.
    #[test]
    fn client_render_shell_is_flagged_but_short_static_pages_are_not() {
        let shell_html = 4000;
        let placeholder = "You need to enable JavaScript to run this app.";
        assert!(super::is_client_rendered(shell_html, true, placeholder));

        // Short page with no scripts: real content, however brief.
        assert!(!super::is_client_rendered(shell_html, false, placeholder));
        // Markup barely larger than its text: not a shell.
        assert!(!super::is_client_rendered(300, true, placeholder));
        // Scripts plus a real article: the text carries the page.
        let article = "x".repeat(200);
        assert!(!super::is_client_rendered(shell_html, true, &article));
    }
    use super::*;
    use crate::testutil::fetch_private;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    struct StaticLookup(Vec<(String, Vec<SocketAddr>)>);

    impl Lookup for StaticLookup {
        fn lookup(&self, host: String) -> LookupFuture {
            let addrs = self
                .0
                .iter()
                .find(|(name, _)| *name == host)
                .map(|(_, addrs)| addrs.clone())
                .unwrap_or_default();
            Box::pin(async move { Ok(addrs) })
        }
    }

    fn resolver_answering(answers: &[(&str, &str)]) -> PublicResolver {
        PublicResolver {
            lookup: Arc::new(StaticLookup(
                answers
                    .iter()
                    .map(|(host, ip)| {
                        (
                            (*host).to_owned(),
                            vec![SocketAddr::new(ip.parse().unwrap(), 0)],
                        )
                    })
                    .collect(),
            )),
            proxy_hosts: Vec::new(),
        }
    }

    /// Answers lookups in order, so one test can show a public answer at the
    /// pre-check and a private one at connect time.
    struct ScriptedLookup(Mutex<VecDeque<io::Result<Vec<SocketAddr>>>>);

    impl Lookup for ScriptedLookup {
        fn lookup(&self, _host: String) -> LookupFuture {
            let next = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()));
            Box::pin(async move { next })
        }
    }

    fn scripted_resolver(
        answers: Vec<io::Result<Vec<SocketAddr>>>,
        proxy_hosts: &[&str],
    ) -> Arc<PublicResolver> {
        Arc::new(PublicResolver {
            lookup: Arc::new(ScriptedLookup(Mutex::new(answers.into()))),
            proxy_hosts: proxy_hosts.iter().map(|host| (*host).to_owned()).collect(),
        })
    }

    fn addr(ip: &str) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::new(ip.parse().unwrap(), 0)])
    }

    #[test]
    fn ip_publicness_table() {
        let public = ["8.8.8.8", "1.1.1.1", "2606:4700::1111"];
        for ip in public {
            assert!(ip_is_public(ip.parse().unwrap()), "{ip} should be public");
        }
        let private = [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1", // v4-mapped loopback
            "::ffff:10.0.0.1",
            "::127.0.0.1",     // v4-compatible loopback
            "::a00:1",         // v4-compatible 10.0.0.1
            "64:ff9b::7f00:1", // NAT64-embedded 127.0.0.1
            "64:ff9b::a00:1",  // NAT64-embedded 10.0.0.1
            "240.0.0.1",       // class E reserved
            "0.1.2.3",         // 0.0.0.0/8
        ];
        for ip in private {
            assert!(!ip_is_public(ip.parse().unwrap()), "{ip} should be private");
        }
    }

    #[tokio::test]
    async fn guard_rejects_schemes_and_private_hosts() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "http://127.0.0.1/admin",
            "http://[::1]:8080/",
            "http://10.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://localhost:3000/",
            "http://foo.localhost/",
        ] {
            let err = crate::testutil::fetch(url, /*allow_private*/ false)
                .await
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("SSRF guard") || msg.contains("only http/https"),
                "{url} → {msg}"
            );
        }
    }

    /// The filter runs on the addresses the connection will use, and it rejects
    /// every non-public class rather than only loopback.
    #[tokio::test]
    async fn the_resolver_refuses_non_public_answers() {
        for (host, ip) in [
            ("loop.test", "127.0.0.1"),
            ("private.test", "10.1.2.3"),
            ("metadata.test", "169.254.169.254"),
            ("cgnat.test", "100.64.0.1"),
            ("v6loop.test", "::1"),
            ("v6link.test", "fe80::1"),
        ] {
            let resolver = resolver_answering(&[(host, ip)]);
            let Err(err) = resolver.resolve(host.parse().unwrap()).await else {
                panic!("{host} -> {ip} should have been refused");
            };
            assert!(err.to_string().contains("SSRF guard"), "{host}: {err}");
        }

        let resolver = resolver_answering(&[("public.test", "93.184.216.34")]);
        let addrs: Vec<SocketAddr> = resolver
            .resolve("public.test".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert_eq!(addrs, vec![SocketAddr::from(([93, 184, 216, 34], 0))]);
    }

    /// End to end through `WebTools`: the pre-check sees a public answer and the
    /// connection's own lookup sees loopback. Only the resolver-level filter can
    /// catch that. This is the regression for the old two-lookup window (guard,
    /// then connect).
    #[tokio::test]
    async fn a_rebinding_answer_is_refused_at_connect_time() {
        let dns = scripted_resolver(vec![addr("93.184.216.34"), addr("127.0.0.1")], &[]);
        let tools = crate::WebTools::with_resolver(None, Duration::from_secs(2), dns).unwrap();
        let err = tools
            .fetch(&json!({"url": "http://rebind.test/x"}))
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("SSRF guard"), "{message}");
        assert!(message.contains("rebind.test"), "{message}");
    }

    /// A name this host cannot resolve is refused at the pre-check, even with a
    /// proxy configured: the proxy might resolve it into the internal network,
    /// and the connection-time filter only ever sees the proxy's own address.
    #[tokio::test]
    async fn an_unresolvable_target_is_refused_instead_of_being_handed_to_a_proxy() {
        let dns = scripted_resolver(vec![Err(io::Error::other("no such host"))], &["proxy.test"]);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::clone(&dns))
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let err = fetch_url(
            &client,
            &dns,
            "http://internal.test/x",
            /*allow_private*/ false,
        )
        .await
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("cannot resolve host"), "{message}");
    }

    /// The proxy endpoint is the user's declared egress route, not a fetch
    /// target: a private or loopback answer must not lock every web tool out
    /// from behind a local/corporate proxy. Ordinary hosts keep the filter.
    #[tokio::test]
    async fn a_configured_proxy_endpoint_may_resolve_privately() {
        let dns = scripted_resolver(vec![addr("10.0.0.5"), addr("10.0.0.6")], &["proxy.test"]);
        let proxy: Vec<SocketAddr> = dns
            .resolve("proxy.test".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert_eq!(proxy, vec![SocketAddr::from(([10, 0, 0, 5], 0))]);

        let Err(err) = dns.resolve("target.test".parse().unwrap()).await else {
            panic!("a non-proxy host must keep the public-address filter");
        };
        assert!(err.to_string().contains("SSRF guard"), "{err}");
    }

    #[test]
    fn proxy_hosts_are_read_like_reqwest_reads_them() {
        let env = |name: &str| match name {
            "HTTPS_PROXY" => Some("http://user:pass@proxy.test:3128".to_string()),
            "all_proxy" => Some("10.0.0.9:1080".to_string()),
            "no_proxy" => Some("*".to_string()),
            _ => None,
        };
        let mut hosts = proxy_hosts(env);
        hosts.sort();
        assert_eq!(hosts, vec!["10.0.0.9", "proxy.test"]);

        // reqwest ignores proxy variables under CGI, to avoid httpoxy.
        let cgi = |name: &str| match name {
            "REQUEST_METHOD" => Some("GET".to_string()),
            "HTTPS_PROXY" => Some("http://proxy.test:3128".to_string()),
            _ => None,
        };
        assert!(proxy_hosts(cgi).is_empty());
    }

    /// The Content-Type is refused before the body is read. The fixture promises
    /// a body it never sends, so an implementation that reads first blocks until
    /// the client timeout instead of reporting the unsupported type.
    #[tokio::test]
    async fn a_binary_content_type_is_refused_before_the_body_is_read() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                      Content-Length: 1048576\r\n\r\n",
                )
                .await
                .unwrap();
            // EOF here means the client dropped the response without waiting
            // for the body.
            let mut byte = [0u8; 1];
            let closed = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte)).await;
            let _ = closed_tx.send(matches!(closed, Ok(Ok(0))));
        });

        let err = crate::testutil::fetch_private_with_timeout(
            &format!("http://{addr}/blob"),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("unsupported content type"),
            "{err:#}"
        );
        assert!(
            closed_rx.await.unwrap(),
            "the response body must not be read"
        );
    }

    #[test]
    fn charset_parameters_and_meta_declarations_are_parsed() {
        assert_eq!(
            charset_from_content_type("text/html; charset=gbk"),
            Some("gbk")
        );
        assert_eq!(
            charset_from_content_type("text/html; charset=\"shift_jis\"; x=y"),
            Some("shift_jis")
        );
        assert_eq!(charset_from_content_type("text/html"), None);

        assert_eq!(
            html_meta_charset(b"<html><head><meta charset=\"gbk\">"),
            Some("gbk")
        );
        assert_eq!(
            html_meta_charset(
                b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=shift_jis\">"
            ),
            Some("shift_jis")
        );
        assert_eq!(
            html_meta_charset(b"<meta name=\"x\"><metadata charset=gbk>"),
            None
        );
        // A commented-out declaration is not authoritative, and must not
        // override the real one after it.
        assert_eq!(
            html_meta_charset(b"<!-- <meta charset=windows-1252> --><meta charset=utf-8>"),
            Some("utf-8")
        );
        assert_eq!(
            html_meta_charset(b"<head><!-- note --><meta http-equiv=\"Content-Type\" content=\"text/html; charset=utf-8\">"),
            Some("utf-8")
        );
        // Only the prescan window counts, as in browsers.
        let mut late = vec![b' '; 1024];
        late.extend_from_slice(b"<meta charset=gbk>");
        assert_eq!(html_meta_charset(&late), None);
    }

    #[tokio::test]
    async fn legacy_charsets_in_the_header_are_decoded() {
        let server = MockServer::start().await;
        for (route, content_type, encoding, text) in [
            (
                "/gbk",
                "text/html; charset=gbk",
                encoding_rs::GBK,
                "中文标题",
            ),
            (
                "/sjis",
                "text/plain; charset=shift_jis",
                encoding_rs::SHIFT_JIS,
                "日本語の見出し",
            ),
        ] {
            let page = format!("<html><body>{text}</body></html>");
            let (bytes, _, _) = encoding.encode(&page);
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(bytes.into_owned(), content_type),
                )
                .mount(&server)
                .await;
            let out = fetch_private(&format!("{}{route}", server.uri()))
                .await
                .unwrap();
            assert!(out.contains(text), "{content_type}: {out}");
            assert!(!out.contains('\u{FFFD}'), "{content_type}: {out}");
        }
    }

    /// With no charset in the header, the HTML declaration is the next source;
    /// the bytes of the declaration itself are ASCII, so the prescan reads the
    /// same bytes the browser would.
    #[tokio::test]
    async fn an_html_meta_charset_is_honoured_without_a_header_charset() {
        let server = MockServer::start().await;
        let text = "中文标题";
        for (route, meta) in [
            ("/meta", "<meta charset=\"gbk\">"),
            (
                "/http-equiv",
                "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=gbk\">",
            ),
        ] {
            let page = format!("<html><head>{meta}</head><body>{text}</body></html>");
            let (bytes, _, _) = encoding_rs::GBK.encode(&page);
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(bytes.into_owned(), "text/html"),
                )
                .mount(&server)
                .await;
            let out = fetch_private(&format!("{}{route}", server.uri()))
                .await
                .unwrap();
            assert!(out.contains(text), "{meta}: {out}");
        }
    }

    /// A commented-out declaration used to win over the real one, decoding a
    /// correct UTF-8 page as windows-1252 with no replacement characters to
    /// warn about — just mojibake.
    #[tokio::test]
    async fn a_commented_out_meta_charset_does_not_override_the_real_one() {
        let server = MockServer::start().await;
        let text = "中文标题";
        let page = format!(
            "<html><head><!-- <meta charset=windows-1252> --><meta charset=utf-8></head>\
             <body>{text}</body></html>"
        );
        Mock::given(method("GET"))
            .and(path("/comment"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(page.into_bytes(), "text/html"))
            .mount(&server)
            .await;
        let out = fetch_private(&format!("{}/comment", server.uri()))
            .await
            .unwrap();
        assert!(out.contains(text), "{out}");
    }

    /// An unknown label still yields text, but the result says the decode was
    /// lossy instead of letting replacement characters look like the page.
    #[tokio::test]
    async fn an_unknown_charset_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/odd"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "hello".as_bytes().to_vec(),
                "text/plain; charset=x-nonesuch",
            ))
            .mount(&server)
            .await;
        let out = fetch_private(&format!("{}/odd", server.uri()))
            .await
            .unwrap();
        assert!(out.starts_with("hello"), "{out}");
        assert!(out.contains("'x-nonesuch' is not recognised"), "{out}");
    }

    #[test]
    fn normalize_url_hygiene() {
        // http upgrades to https when the guard is active.
        let url = normalize_url("http://example.com/x", /*upgrade_http*/ true).unwrap();
        assert_eq!(url.as_str(), "https://example.com/x");
        // ...but not in test mode.
        let url = normalize_url("http://example.com/x", /*upgrade_http*/ false).unwrap();
        assert_eq!(url.scheme(), "http");

        let err = normalize_url("https://user:pw@example.com/", true).unwrap_err();
        assert!(format!("{err:#}").contains("embedded credentials"));

        let long = format!("https://example.com/{}", "a".repeat(2000));
        let err = normalize_url(&long, true).unwrap_err();
        assert!(format!("{err:#}").contains("longer than 2000"));
    }

    #[tokio::test]
    async fn cross_host_redirects_are_reported_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/away"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "https://elsewhere.example/x"),
            )
            .mount(&server)
            .await;
        let out = fetch_private(&format!("{}/away", server.uri()))
            .await
            .unwrap();
        assert!(out.starts_with("Redirect detected:"), "{out}");
        assert!(out.contains("https://elsewhere.example/x"));
    }

    #[tokio::test]
    async fn fetches_plain_text_and_converts_html() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/plain"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello plain"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<html><head><title>T</title><script>evil()</script></head>\
                 <body><h1>Header</h1><p>Body &amp; more</p></body></html>",
                "text/html",
            ))
            .mount(&server)
            .await;

        let out = fetch_private(&format!("{}/plain", server.uri()))
            .await
            .unwrap();
        assert_eq!(out, "hello plain");

        let out = fetch_private(&format!("{}/page", server.uri()))
            .await
            .unwrap();
        assert!(out.contains("Header"));
        assert!(out.contains("Body & more"));
        assert!(!out.contains("evil"), "script content stripped: {out}");
        assert!(!out.contains('<'), "no tags remain: {out}");
    }

    #[tokio::test]
    async fn follows_redirects_and_bounds_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/from"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/to"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/to"))
            .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/loop"))
            .mount(&server)
            .await;

        let out = fetch_private(&format!("{}/from", server.uri()))
            .await
            .unwrap();
        // A followed redirect is reported: the body came from /to, and the
        // caller has to be able to tell that from the URL it asked for.
        assert!(
            out.starts_with("landed") && out.contains("/to]"),
            "expected the redirect target to be reported: {out}"
        );
        // The same URL fetched directly carries no such note.
        let direct = fetch_private(&format!("{}/to", server.uri()))
            .await
            .unwrap();
        assert_eq!(direct, "landed");

        let err = fetch_private(&format!("{}/loop", server.uri()))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("too many redirects"));
    }

    /// The 429 text asks the model to respect Retry-After, so the header value
    /// has to reach it — and a value that cannot be one (a legal one is
    /// delta-seconds or an HTTP-date) is reported as ignored rather than
    /// repeated as fact.
    #[tokio::test]
    async fn rate_limit_reports_retry_after_and_rejects_an_unusable_value() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/limited"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "120"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/absurd"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "9".repeat(200)))
            .mount(&server)
            .await;

        let err = fetch_private(&format!("{}/limited", server.uri()))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("Retry-After: 120"));

        let err = fetch_private(&format!("{}/absurd", server.uri()))
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("Retry-After: (value too long to be one; ignored)"),
            "{message}"
        );
        assert!(!message.contains(&"9".repeat(8)), "{message}");
    }

    /// Text-ish `application/*` types are read; a genuinely binary one is still
    /// refused rather than decoded lossily.
    #[tokio::test]
    async fn application_text_types_are_read_and_binary_is_not() {
        let server = MockServer::start().await;
        for (route, content_type) in [
            ("/config.yaml", "application/x-yaml"),
            ("/script.sh", "application/x-sh"),
            ("/notes.toml", "application/toml; charset=utf-8"),
            ("/schema.json", "application/schema+json"),
            ("/rows.ndjson", "application/x-ndjson"),
            ("/seq.json", "application/json-seq"),
            ("/legacy.js", "application/x-javascript"),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("Content-Type", content_type)
                        .set_body_string("key: value"),
                )
                .mount(&server)
                .await;
            let out = fetch_private(&format!("{}{route}", server.uri()))
                .await
                .unwrap();
            assert_eq!(out, "key: value", "{content_type}");
        }
        // These are binary (or carry a marker in a parameter): they keep the
        // unsupported-type answer rather than arriving as lossy noise.
        for (route, content_type) in [
            ("/sharedlib", "application/x-sharedlib"),
            ("/flash", "application/x-shockwave-flash"),
            ("/sqlite", "application/x-sqlite3"),
            (
                "/docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            ("/octet", "application/octet-stream; x=json"),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(vec![0u8, 159, 146, 150], content_type),
                )
                .mount(&server)
                .await;
            let err = fetch_private(&format!("{}{route}", server.uri()))
                .await
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("unsupported content type"),
                "{content_type}: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn errors_on_status_and_binary_content() {
        let server = MockServer::start().await;
        let cases = [
            ("/unauthorized", 401, "Authentication is required"),
            ("/forbidden", 403, "do not bypass the refusal"),
            ("/missing", 404, "hidden from unauthenticated requests"),
            ("/gone", 410, "resource is gone"),
            ("/limited", 429, "rate-limited"),
            ("/error", 500, "a later retry may help"),
            ("/gateway", 502, "a later retry may help"),
            ("/unavailable", 503, "a later retry may help"),
            ("/timeout", 504, "a later retry may help"),
            ("/method", 405, "Confirm the requested resource"),
        ];
        for &(route, status, _) in &cases {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_string("error body must not become review evidence"),
                )
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/blob"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(vec![0u8, 159, 146, 150], "application/octet-stream"),
            )
            .mount(&server)
            .await;

        for &(route, status, guidance) in &cases {
            let url = format!("{}{route}", server.uri());
            let err = fetch_private(&url).await.unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains(&format!("HTTP {status}")), "{message}");
            assert!(message.contains(&url), "{message}");
            assert!(message.contains(guidance), "{message}");
            if status == 404 {
                // Git advice is keyed on the URL shape, not on every 404: a
                // path that is not PR-shaped gets the generic text only.
                assert!(!message.contains("Git remotes"), "{message}");
                assert!(!message.contains("git ls-remote"), "{message}");
            }
            assert!(!message.contains("error body must not become review evidence"));
        }

        // The PR shape is the case the Git hints exist for.
        Mock::given(method("GET"))
            .and(path("/team/project/pulls/42"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = fetch_private(&format!("{}/team/project/pulls/42", server.uri()))
            .await
            .unwrap_err();
        let message = format!("{err:#}");
        for hint in [
            "This looks like a pull request URL",
            "current checkout's Git remotes",
            "git ls-remote",
            "git fetch for missing objects",
            "PR head refs do not establish the target branch",
            "Avoid git pull",
        ] {
            assert!(message.contains(hint), "{message}");
        }

        let err = fetch_private(&format!("{}/blob", server.uri()))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unsupported content type"));
    }

    /// A body far past the old 50k clip comes back whole. Bounding what the
    /// model sees belongs to core's offload seam, which keeps the rest on disk;
    /// clipping here used to delete it instead.
    #[tokio::test]
    async fn long_text_is_returned_whole() {
        let server = MockServer::start().await;
        let body = "x".repeat(600_000);
        Mock::given(method("GET"))
            .and(path("/long"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
            .mount(&server)
            .await;
        let out = fetch_private(&format!("{}/long", server.uri()))
            .await
            .unwrap();
        assert_eq!(out, body);
    }

    /// The marker is derived from the bound (a literal would go stale if the
    /// bound moved) and says the remainder is not retrievable, so the model does
    /// not read a second fetch as a way to get it.
    #[test]
    fn the_truncation_marker_names_the_bound_and_says_the_rest_is_gone() {
        let marker = super::truncation_marker();
        assert!(
            marker.contains(&format!(
                "at {} MiB",
                limits::MAX_DOWNLOAD_BYTES / (1024 * 1024)
            )),
            "{marker}"
        );
        assert!(marker.contains("cannot retrieve it"), "{marker}");
        assert!(
            !marker.contains("content truncated"),
            "the marker is about the download, not the text: {marker}"
        );
    }

    /// End to end, over the bound: the transfer stops there, everything up to it
    /// is kept, and the marker is appended. Ignored by default because it pushes
    /// the whole bound (20 MiB) through the mock server; run it explicitly with
    /// `cargo test -p kloop-web -- --ignored`.
    #[ignore = "transfers the full 20 MiB bound; run with --ignored"]
    #[tokio::test]
    async fn the_download_cap_still_bounds_the_transfer_and_is_reported() {
        let server = MockServer::start().await;
        let body = vec![b'x'; limits::MAX_DOWNLOAD_BYTES + 1024];
        Mock::given(method("GET"))
            .and(path("/oversized"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/plain"))
            .mount(&server)
            .await;

        let out = fetch_private(&format!("{}/oversized", server.uri()))
            .await
            .unwrap();
        let marker = super::truncation_marker();
        let cut = out.find(&marker).expect("truncation is reported");
        assert_eq!(
            out[..cut].len(),
            limits::MAX_DOWNLOAD_BYTES,
            "everything downloaded is kept",
        );
        assert_eq!(out[cut..], marker, "the marker is the whole tail");
    }
}
