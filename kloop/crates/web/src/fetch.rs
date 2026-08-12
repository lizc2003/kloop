//! web_fetch: manual redirect following with a per-hop SSRF guard, download
//! and text caps, HTML→text conversion.

use std::net::IpAddr;
use std::net::Ipv4Addr;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use reqwest::Url;

use crate::limits;

const MAX_REDIRECTS: usize = 5;

pub(crate) async fn fetch_url(
    client: &reqwest::Client,
    raw_url: &str,
    allow_private: bool,
) -> Result<String> {
    let mut url = normalize_url(raw_url, /*upgrade_http*/ !allow_private)?;
    for _ in 0..=MAX_REDIRECTS {
        guard(&url, allow_private).await?;
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
            bail!("web_fetch: HTTP {} for {url}", resp.status());
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        return read_body(resp, &content_type, &url).await;
    }
    bail!("web_fetch: too many redirects (more than {MAX_REDIRECTS})")
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
    let raw = String::from_utf8_lossy(&bytes);
    let is_html = content_type.contains("text/html")
        || content_type.contains("application/xhtml")
        || (content_type.is_empty() && raw.trim_start().starts_with('<'));
    let is_text = content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.is_empty();
    if !is_html && !is_text {
        bail!(
            "web_fetch: unsupported content type '{content_type}' at {url} (text-like content only)"
        );
    }
    let mut text = if is_html {
        crate::html::html_to_text(&raw)
    } else {
        raw.into_owned()
    };
    if text.is_empty() {
        text = "(empty response body)".into();
    }
    let (mut clipped, text_truncated) = limits::truncate_chars(text, limits::MAX_TEXT_CHARS);
    if text_truncated {
        clipped.push_str("\n\n[content truncated at 50000 characters]");
    }
    if download_truncated {
        clipped.push_str("\n\n[download truncated at 5MB]");
    }
    Ok(clipped)
}

/// Scheme and address policy. Every redirect hop passes through here; DNS
/// names are resolved and all addresses must be public (best-effort — the
/// actual connection re-resolves, accepted TOCTOU).
async fn guard(url: &Url, allow_private: bool) -> Result<()> {
    match url.scheme() {
        "http" | "https" => {}
        other => bail!("web_fetch: only http/https URLs are supported (got '{other}:')"),
    }
    if allow_private {
        return Ok(());
    }
    let refuse = |what: &dyn std::fmt::Display| {
        format!("web_fetch: {what} is a private/internal address; refusing (SSRF guard)")
    };
    match url.host() {
        None => bail!("web_fetch: URL has no host"),
        Some(url::Host::Ipv4(ip)) if !ip_is_public(IpAddr::V4(ip)) => bail!(refuse(&ip)),
        Some(url::Host::Ipv6(ip)) if !ip_is_public(IpAddr::V6(ip)) => bail!(refuse(&ip)),
        Some(url::Host::Domain(name)) => {
            if name.eq_ignore_ascii_case("localhost")
                || name.to_ascii_lowercase().ends_with(".localhost")
            {
                bail!(refuse(&name));
            }
            let port = url.port_or_known_default().unwrap_or(443);
            let addrs: Vec<IpAddr> = tokio::net::lookup_host((name, port))
                .await
                .with_context(|| format!("web_fetch: cannot resolve host '{name}'"))?
                .map(|sa| sa.ip())
                .collect();
            if addrs.is_empty() {
                bail!("web_fetch: host '{name}' resolves to no addresses");
            }
            if addrs.iter().any(|ip| !ip_is_public(*ip)) {
                bail!(refuse(&name));
            }
        }
        Some(_) => {}
    }
    Ok(())
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
    use super::*;
    use crate::testutil::fetch_private;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

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
        let client = crate::testutil::client();
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
            let err = fetch_url(&client, url, /*allow_private*/ false)
                .await
                .unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("SSRF guard") || msg.contains("only http/https"),
                "{url} → {msg}"
            );
        }
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
        assert_eq!(out, "landed");

        let err = fetch_private(&format!("{}/loop", server.uri()))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("too many redirects"));
    }

    #[tokio::test]
    async fn errors_on_status_and_binary_content() {
        let server = MockServer::start().await;
        let cases = [
            ("/unauthorized", 401),
            ("/forbidden", 403),
            ("/missing", 404),
            ("/error", 500),
        ];
        for &(route, status) in &cases {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(status))
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

        for &(route, status) in &cases {
            let err = fetch_private(&format!("{}{route}", server.uri()))
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains(&format!("HTTP {status}")));
        }

        let err = fetch_private(&format!("{}/blob", server.uri()))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unsupported content type"));
    }

    #[tokio::test]
    async fn long_text_is_clipped_with_a_note() {
        let server = MockServer::start().await;
        let body = "x".repeat(60_000);
        Mock::given(method("GET"))
            .and(path("/long"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let out = fetch_private(&format!("{}/long", server.uri()))
            .await
            .unwrap();
        assert!(
            out.ends_with("[content truncated at 50000 characters]"),
            "note appended"
        );
        assert!(out.len() < 51_000);
    }

    #[tokio::test]
    async fn download_and_text_caps_are_reported_independently() {
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
        assert!(
            out.contains("[content truncated at 50000 characters]"),
            "{out:?}"
        );
        assert!(out.ends_with("[download truncated at 5MB]"), "{out:?}");
        assert!(out.len() < 51_000);
    }
}
